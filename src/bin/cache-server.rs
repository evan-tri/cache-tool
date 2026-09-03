#![allow(clippy::result_large_err)]

use clap::Parser;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};
use uuid::Uuid;

use cache_tool::cache::cache_server::{Cache, CacheServer};
use cache_tool::cache::{
    CheckRequest, CheckResponse, GetRequest, GetResponse, PutRequest, PutResponse, put_request,
};

const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "cache-server",
    version,
    about = "Remote cache server for cache-tool"
)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:50051")]
    addr: String,

    /// Directory holding blobs/, tmp/ and metadata.db.
    #[arg(long, default_value = "./cache-storage")]
    storage: PathBuf,

    /// Evict least-recently-used blobs once the total exceeds this many GB.
    #[arg(long, default_value_t = 10.0)]
    max_size_gb: f64,

    /// Require this key on every request. Omit to run without authentication.
    /// Prefer the environment variable: command lines are visible to every
    /// process on the machine.
    #[arg(long, env = "CACHE_SERVER_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Clone)]
struct RequireApiKey(Option<String>);

impl tonic::service::Interceptor for RequireApiKey {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let Some(expected) = &self.0 else {
            return Ok(request);
        };

        let presented = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));

        match presented {
            Some(key) if constant_time_eq(key, expected) => Ok(request),
            _ => Err(Status::unauthenticated("missing or invalid API key")),
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
struct Db(Arc<Mutex<Connection>>);

impl Db {
    fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;

        conn.pragma_update(None, "journal_mode", "WAL")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS blobs (
                 hash        TEXT PRIMARY KEY,
                 size        INTEGER NOT NULL,
                 last_access INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS blobs_lru ON blobs(last_access);",
        )?;

        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    fn reconcile(&self, blobs: &Path) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();

        let known: Vec<String> = {
            let mut stmt = conn.prepare("SELECT hash FROM blobs")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for hash in known {
            if !blobs.join(&hash).exists() {
                conn.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])?;
            }
        }

        if let Ok(entries) = std::fs::read_dir(blobs) {
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                conn.execute(
                    "INSERT OR IGNORE INTO blobs (hash, size, last_access)
                     VALUES (?1, ?2, ?3)",
                    params![name, meta.len() as i64, now()],
                )?;
            }
        }

        Ok(())
    }

    async fn touch(&self, hash: String) {
        let db = self.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db.0.lock().unwrap();
            conn.execute(
                "UPDATE blobs SET last_access = ?1 WHERE hash = ?2",
                params![now(), hash],
            )
        })
        .await;
    }

    async fn record(&self, hash: String, size: u64) {
        let db = self.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db.0.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO blobs (hash, size, last_access)
                 VALUES (?1, ?2, ?3)",
                params![hash, size as i64, now()],
            )
        })
        .await;
    }

    async fn evict(&self, blobs: PathBuf, max_bytes: u64) {
        let db = self.clone();
        let _ = tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = db.0.lock().unwrap();

            let total: i64 =
                conn.query_row("SELECT COALESCE(SUM(size), 0) FROM blobs", [], |r| r.get(0))?;

            let mut excess = (total as u64).saturating_sub(max_bytes);
            if excess == 0 {
                return Ok(());
            }

            let victims: Vec<(String, i64)> = {
                let mut stmt =
                    conn.prepare("SELECT hash, size FROM blobs ORDER BY last_access ASC")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                rows.filter_map(|r| r.ok()).collect()
            };

            for (hash, size) in victims {
                if excess == 0 {
                    break;
                }
                match std::fs::remove_file(blobs.join(&hash)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => continue,
                }
                conn.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])?;
                excess = excess.saturating_sub(size as u64);
                eprintln!("cache-server: evicted {}", &hash[..12.min(hash.len())]);
            }

            Ok(())
        })
        .await;
    }
}

struct CacheService {
    blobs: PathBuf,
    tmp: PathBuf,
    db: Db,
    max_bytes: u64,
}

impl CacheService {
    fn blob_path(&self, hash: &str) -> Result<PathBuf, Status> {
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Status::invalid_argument("hash must be 64 hex characters"));
        }
        Ok(self.blobs.join(hash))
    }
}

async fn discard(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

// Takes `file` by value so it is closed on every exit path before the caller
// runs cleanup. Windows refuses to delete a file that is still open, and
// discard() ignores that failure by design, so a temp file written by a
// rejected upload would leak. Unix would have hidden this: unlink succeeds
// while a handle is open.
async fn stream_into(
    stream: &mut tonic::Streaming<PutRequest>,
    mut file: tokio::fs::File,
) -> Result<u64, Status> {
    let mut written: u64 = 0;

    loop {
        let msg = match stream.message().await {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(e) => return Err(e),
        };

        match msg.data {
            Some(put_request::Data::Chunk(bytes)) => {
                file.write_all(&bytes)
                    .await
                    .map_err(|e| Status::internal(format!("write failed: {e}")))?;
                written += bytes.len() as u64;
            }
            _ => {
                return Err(Status::invalid_argument(
                    "expected a chunk after the hash message",
                ));
            }
        }
    }

    file.sync_all()
        .await
        .map_err(|e| Status::internal(format!("sync failed: {e}")))?;

    Ok(written)
}

#[tonic::async_trait]
impl Cache for CacheService {
    async fn check(
        &self,
        request: Request<CheckRequest>,
    ) -> Result<Response<CheckResponse>, Status> {
        let hash = request.into_inner().hash;

        let exists = tokio::fs::metadata(self.blob_path(&hash)?).await.is_ok();

        if exists {
            self.db.touch(hash).await;
        }

        Ok(Response::new(CheckResponse { exists }))
    }

    type GetStream = ReceiverStream<Result<GetResponse, Status>>;

    async fn get(&self, request: Request<GetRequest>) -> Result<Response<Self::GetStream>, Status> {
        let hash = request.into_inner().hash;
        let path = self.blob_path(&hash)?;

        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|_| Status::not_found("blob not found"))?;

        self.db.touch(hash).await;

        let (tx, rx) = tokio::sync::mpsc::channel(4);

        tokio::spawn(async move {
            let mut buf = vec![0u8; CHUNK_SIZE];
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        if tx.send(Ok(GetResponse { chunk })).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn put(
        &self,
        request: Request<tonic::Streaming<PutRequest>>,
    ) -> Result<Response<PutResponse>, Status> {
        let mut stream = request.into_inner();

        let claimed = match stream.message().await? {
            Some(PutRequest {
                data: Some(put_request::Data::Hash(h)),
            }) => h,
            _ => {
                return Err(Status::invalid_argument(
                    "first message must contain the hash",
                ));
            }
        };

        let final_path = self.blob_path(&claimed)?;

        if tokio::fs::metadata(&final_path).await.is_ok() {
            self.db.touch(claimed).await;
            return Ok(Response::new(PutResponse { ok: true }));
        }

        let tmp_path = self.tmp.join(Uuid::new_v4().to_string());
        let file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| Status::internal(format!("cannot open temp file: {e}")))?;

        let written = match stream_into(&mut stream, file).await {
            Ok(written) => written,
            Err(e) => {
                discard(&tmp_path).await;
                return Err(e);
            }
        };

        if tokio::fs::metadata(&final_path).await.is_ok() {
            discard(&tmp_path).await;
            self.db.touch(claimed).await;
            return Ok(Response::new(PutResponse { ok: true }));
        }

        if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
            discard(&tmp_path).await;
            return Err(Status::internal(format!("rename failed: {e}")));
        }

        self.db.record(claimed, written).await;
        self.db.evict(self.blobs.clone(), self.max_bytes).await;

        Ok(Response::new(PutResponse { ok: true }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let blobs = args.storage.join("blobs");
    let tmp = args.storage.join("tmp");
    std::fs::create_dir_all(&blobs)?;
    std::fs::create_dir_all(&tmp)?;

    for entry in std::fs::read_dir(&tmp)? {
        std::fs::remove_file(entry?.path())?;
    }

    let db = Db::open(&args.storage.join("metadata.db"))?;
    db.reconcile(&blobs)?;

    let max_bytes = (args.max_size_gb * 1024.0 * 1024.0 * 1024.0) as u64;

    let service = CacheService {
        blobs: blobs.clone(),
        tmp,
        db: db.clone(),
        max_bytes,
    };

    db.evict(blobs, max_bytes).await;

    let addr = args.addr.parse()?;
    println!(
        "cache-server listening on {addr} (storage {}, cap {:.1} GB, auth {})",
        args.storage.display(),
        args.max_size_gb,
        if args.api_key.is_some() {
            "required"
        } else {
            "disabled"
        }
    );

    Server::builder()
        .add_service(CacheServer::with_interceptor(
            service,
            RequireApiKey(args.api_key),
        ))
        .serve(addr)
        .await?;

    Ok(())
}
