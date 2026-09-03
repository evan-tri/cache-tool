// The gRPC helpers below return tonic::Status, which exceeds the lint threshold
// on its own. Boxing it would diverge from the signatures tonic generates.
#![allow(clippy::result_large_err)]

mod common;

use common::{TestClient, TestServer, gb, test_hash};

use cache_tool::cache::{CheckRequest, GetRequest, PutRequest, PutResponse, put_request};
use tonic::Code;

const CHUNK_SIZE: usize = 64 * 1024;

async fn put_blob(
    client: &mut TestClient,
    hash: &str,
    data: &[u8],
) -> Result<PutResponse, tonic::Status> {
    let mut messages = vec![PutRequest {
        data: Some(put_request::Data::Hash(hash.to_string())),
    }];
    for chunk in data.chunks(CHUNK_SIZE) {
        messages.push(PutRequest {
            data: Some(put_request::Data::Chunk(chunk.to_vec())),
        });
    }
    client
        .put(tokio_stream::iter(messages))
        .await
        .map(|response| response.into_inner())
}

async fn get_blob(client: &mut TestClient, hash: &str) -> Result<Vec<u8>, tonic::Status> {
    let mut stream = client
        .get(GetRequest {
            hash: hash.to_string(),
        })
        .await?
        .into_inner();

    let mut bytes = Vec::new();
    while let Some(response) = stream.message().await? {
        bytes.extend_from_slice(&response.chunk);
    }
    Ok(bytes)
}

async fn check(client: &mut TestClient, hash: &str) -> Result<bool, tonic::Status> {
    client
        .check(CheckRequest {
            hash: hash.to_string(),
        })
        .await
        .map(|response| response.into_inner().exists)
}

// ---------------------------------------------------------------- Check

#[tokio::test]
async fn check_is_false_for_an_unknown_hash() {
    let server = TestServer::start();
    let mut client = server.client().await;

    let exists = check(&mut client, &test_hash("never-uploaded"))
        .await
        .expect("check call should succeed");

    assert!(!exists);
}

#[tokio::test]
async fn check_is_true_after_a_successful_put() {
    let server = TestServer::start();
    let mut client = server.client().await;
    let hash = test_hash("uploaded");

    put_blob(&mut client, &hash, b"artifact bytes")
        .await
        .expect("put should succeed");

    assert!(
        check(&mut client, &hash)
            .await
            .expect("check should succeed")
    );
}

// ---------------------------------------------------------------- Get

#[tokio::test]
async fn get_returns_the_exact_bytes_that_were_put() {
    let server = TestServer::start();
    let mut client = server.client().await;
    let hash = test_hash("round-trip");
    let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();

    put_blob(&mut client, &hash, &payload)
        .await
        .expect("put should succeed");

    let fetched = get_blob(&mut client, &hash)
        .await
        .expect("get should succeed");

    assert_eq!(fetched, payload);
}

#[tokio::test]
async fn get_is_not_found_for_an_unknown_hash() {
    let server = TestServer::start();
    let mut client = server.client().await;

    let error = get_blob(&mut client, &test_hash("absent"))
        .await
        .expect_err("get of an absent blob must fail");

    assert_eq!(error.code(), Code::NotFound);
}

#[tokio::test]
async fn a_blob_larger_than_one_chunk_round_trips() {
    let server = TestServer::start();
    let mut client = server.client().await;
    let hash = test_hash("multi-chunk");

    // Deliberately not a multiple of CHUNK_SIZE, so the final chunk is partial
    // and any off-by-one at the boundary shows up as a length mismatch.
    let payload: Vec<u8> = (0..(CHUNK_SIZE * 5 + 1234))
        .map(|i| (i % 251) as u8)
        .collect();

    put_blob(&mut client, &hash, &payload)
        .await
        .expect("put should succeed");

    let fetched = get_blob(&mut client, &hash)
        .await
        .expect("get should succeed");

    assert_eq!(fetched.len(), payload.len());
    assert_eq!(fetched, payload);
}

// ---------------------------------------------------- Untrusted input

#[tokio::test]
async fn a_hash_containing_path_traversal_is_rejected() {
    let server = TestServer::start();
    let mut client = server.client().await;

    for attempt in [
        "../../etc/passwd",
        "..\\..\\windows\\system32",
        "../tmp/escape",
    ] {
        let error = check(&mut client, attempt)
            .await
            .expect_err("traversal attempt must be rejected");
        assert_eq!(error.code(), Code::InvalidArgument, "for {attempt}");
    }

    assert_eq!(server.blob_count(), 0);
}

#[tokio::test]
async fn a_hash_that_is_not_64_hex_characters_is_rejected() {
    let server = TestServer::start();
    let mut client = server.client().await;

    let valid = test_hash("valid");
    let too_short = &valid[..63];
    let too_long = format!("{valid}a");
    let not_hex = "z".repeat(64);

    for attempt in ["", "abc", too_short, &too_long, &not_hex] {
        let error = check(&mut client, attempt)
            .await
            .expect_err("malformed hash must be rejected");
        assert_eq!(error.code(), Code::InvalidArgument, "for {attempt:?}");
    }

    assert!(check(&mut client, &valid).await.is_ok());
}

#[tokio::test]
async fn a_put_that_does_not_begin_with_a_hash_message_is_rejected() {
    let server = TestServer::start();
    let mut client = server.client().await;

    let messages = vec![PutRequest {
        data: Some(put_request::Data::Chunk(vec![1, 2, 3])),
    }];

    let error = client
        .put(tokio_stream::iter(messages))
        .await
        .expect_err("a stream starting with a chunk must be rejected");

    assert_eq!(error.code(), Code::InvalidArgument);
    assert_eq!(server.blob_count(), 0);
}

// ------------------------------------------------------------ Write-once

#[tokio::test]
async fn putting_an_existing_hash_again_is_accepted_without_overwriting() {
    let server = TestServer::start();
    let mut client = server.client().await;
    let hash = test_hash("write-once");

    put_blob(&mut client, &hash, b"original contents")
        .await
        .expect("first put should succeed");

    let response = put_blob(&mut client, &hash, b"replacement contents")
        .await
        .expect("second put should be accepted");

    assert!(response.ok);
    assert_eq!(server.blob_bytes(&hash), b"original contents");
}

#[tokio::test]
async fn a_failed_upload_leaves_nothing_in_blobs() {
    let server = TestServer::start();
    let mut client = server.client().await;
    let hash = test_hash("abandoned");

    // A second Hash message where a Chunk is required: the server must reject
    // the stream and remove the partial temp file it had already written.
    let messages = vec![
        PutRequest {
            data: Some(put_request::Data::Hash(hash.clone())),
        },
        PutRequest {
            data: Some(put_request::Data::Chunk(vec![7; 1024])),
        },
        PutRequest {
            data: Some(put_request::Data::Hash(hash.clone())),
        },
    ];

    let error = client
        .put(tokio_stream::iter(messages))
        .await
        .expect_err("malformed stream must be rejected");

    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(!server.has_blob(&hash));
    assert_eq!(server.blob_count(), 0);
    assert_eq!(server.tmp_count(), 0, "temp file was not cleaned up");
}

// ------------------------------------------------------------- Eviction

#[tokio::test]
async fn eviction_removes_the_least_recently_used_blob() {
    // Cap fits one 1000-byte blob but not two.
    let server = TestServer::start_with(None, Some(gb(1500)));
    let mut client = server.client().await;

    let first = test_hash("evict-first");
    let second = test_hash("evict-second");

    put_blob(&mut client, &first, &[1u8; 1000])
        .await
        .expect("put first");
    assert!(server.has_blob(&first));

    put_blob(&mut client, &second, &[2u8; 1000])
        .await
        .expect("put second");

    assert!(
        !server.has_blob(&first),
        "oldest blob should have been evicted"
    );
    assert!(server.has_blob(&second));
}

#[tokio::test]
async fn a_recently_read_blob_survives_eviction() {
    // Cap fits two 1000-byte blobs but not three.
    let server = TestServer::start_with(None, Some(gb(2500)));
    let mut client = server.client().await;

    let a = test_hash("lru-a");
    let b = test_hash("lru-b");
    let c = test_hash("lru-c");

    put_blob(&mut client, &a, &[1u8; 1000])
        .await
        .expect("put a");
    put_blob(&mut client, &b, &[2u8; 1000])
        .await
        .expect("put b");

    // Reading A makes it the most recently used, so B becomes the oldest.
    get_blob(&mut client, &a).await.expect("get a");

    put_blob(&mut client, &c, &[3u8; 1000])
        .await
        .expect("put c");

    assert!(
        server.has_blob(&a),
        "A was read most recently and must survive"
    );
    assert!(
        !server.has_blob(&b),
        "B was least recently used and must be evicted"
    );
    assert!(server.has_blob(&c));
}

#[tokio::test]
async fn the_index_is_rebuilt_when_metadata_db_is_deleted() {
    let mut server = TestServer::start_with(None, Some(gb(2500)));
    let mut client = server.client().await;

    let a = test_hash("rebuild-a");
    let b = test_hash("rebuild-b");
    put_blob(&mut client, &a, &[1u8; 1000])
        .await
        .expect("put a");
    put_blob(&mut client, &b, &[2u8; 1000])
        .await
        .expect("put b");

    server.stop();
    server.delete_index();
    server.start_again();

    let mut client = server.client().await;

    // Blobs survive an index loss, because the directory is the source of truth.
    assert_eq!(get_blob(&mut client, &a).await.expect("get a").len(), 1000);
    assert!(check(&mut client, &b).await.expect("check b"));

    // And the rebuilt index is functional: eviction still enforces the cap.
    let c = test_hash("rebuild-c");
    put_blob(&mut client, &c, &[3u8; 1000])
        .await
        .expect("put c");
    assert!(server.blob_count() <= 2, "cap should still be enforced");
    assert!(server.has_blob(&c));
}

// ----------------------------------------------------------------- Auth

#[tokio::test]
async fn a_request_without_a_key_is_rejected_when_auth_is_enabled() {
    let server = TestServer::start_with(Some("correct-key"), None);
    let mut client = server.client().await;

    let error = check(&mut client, &test_hash("anything"))
        .await
        .expect_err("unauthenticated request must be rejected");

    assert_eq!(error.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_request_with_the_wrong_key_is_rejected() {
    let server = TestServer::start_with(Some("correct-key"), None);
    let mut client = server.client_with_key(Some("wrong-key")).await;

    let error = check(&mut client, &test_hash("anything"))
        .await
        .expect_err("wrong key must be rejected");

    assert_eq!(error.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_request_with_the_correct_key_succeeds() {
    let server = TestServer::start_with(Some("correct-key"), None);
    let mut client = server.client_with_key(Some("correct-key")).await;
    let hash = test_hash("authorised");

    put_blob(&mut client, &hash, b"payload")
        .await
        .expect("put with the correct key should succeed");

    assert_eq!(get_blob(&mut client, &hash).await.expect("get"), b"payload");
}

#[tokio::test]
async fn auth_applies_to_every_rpc() {
    let server = TestServer::start_with(Some("correct-key"), None);
    let mut client = server.client().await;
    let hash = test_hash("guarded");

    assert_eq!(
        check(&mut client, &hash).await.expect_err("check").code(),
        Code::Unauthenticated
    );
    assert_eq!(
        get_blob(&mut client, &hash).await.expect_err("get").code(),
        Code::Unauthenticated
    );
    assert_eq!(
        put_blob(&mut client, &hash, b"x")
            .await
            .expect_err("put")
            .code(),
        Code::Unauthenticated
    );
}
