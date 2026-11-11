use blake3::Hasher;
use clap::Parser;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;
use walkdir::WalkDir;

use tonic::codegen::InterceptedService;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Certificate, Channel, ClientTlsConfig};

use cache_tool::cache::cache_client::CacheClient;
use cache_tool::cache::{CheckRequest, GetRequest, PutRequest, put_request};

type Error = Box<dyn std::error::Error>;

const CHUNK_SIZE: usize = 64 * 1024;

const IGNORED_DIRS: [&str; 4] = [".git", "target", "node_modules", "cache-storage"];

#[derive(Parser, Debug)]
#[command(name = "cache-tool", version, about = "A distributed build cache")]
struct Args {
    /// File or directory produced by the build. Repeat for multiple outputs.
    #[arg(long = "outputs", required = true, num_args = 1)]
    outputs: Vec<String>,

    /// Remote cache server, e.g. http://127.0.0.1:50051
    #[arg(long = "remote")]
    remote: Option<String>,

    /// API key for the remote server, if it requires one. Prefer the
    /// environment variable: command lines are visible to other processes.
    #[arg(long = "api-key", env = "CACHE_TOOL_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Where to keep the local cache. Defaults to the platform cache directory.
    #[arg(long = "cache-dir", env = "CACHE_TOOL_CACHE_DIR")]
    cache_dir: Option<PathBuf>,

    /// Trust this CA certificate (PEM) for an https:// remote, instead of the
    /// system trust store. Use this for a self-signed or private CA cert.
    #[arg(long = "ca-cert")]
    ca_cert: Option<PathBuf>,

    /// The build command, given after `--`.
    #[arg(last = true, required = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn normalize(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    s.strip_prefix("./").unwrap_or(&s).to_string()
}

fn should_skip(relative: &str, outputs: &[String]) -> bool {
    if relative.split('/').any(|part| IGNORED_DIRS.contains(&part)) {
        return true;
    }

    outputs.iter().any(|out| {
        let out = normalize(Path::new(out));
        relative == out || relative.starts_with(&format!("{out}/"))
    })
}

fn collect_inputs(args: &Args, root: &Path) -> Vec<(String, PathBuf)> {
    let mut files: Vec<(String, PathBuf)> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let relative = normalize(e.path().strip_prefix(root).unwrap_or(e.path()));
            if should_skip(&relative, &args.outputs) {
                None
            } else {
                Some((relative, e.path().to_path_buf()))
            }
        })
        .collect();

    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn compute_hash(args: &Args, root: &Path) -> std::io::Result<String> {
    let files = collect_inputs(args, root);

    let mut hasher = Hasher::new();

    hasher.update(b"cache-tool-v1\0");

    for arg in &args.command {
        hasher.update(arg.as_bytes());
        hasher.update(&[0]);
    }
    for out in &args.outputs {
        hasher.update(out.as_bytes());
        hasher.update(&[0]);
    }

    for (relative, path) in &files {
        let bytes = std::fs::read(path)?;

        hasher.update(relative.as_bytes());
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

fn cache_paths(hash: &str, cache_dir: Option<&Path>) -> Result<(PathBuf, PathBuf), Error> {
    let base = match cache_dir {
        Some(dir) => dir.to_path_buf(),
        None => dirs::cache_dir()
            .ok_or("no user cache directory on this platform")?
            .join("cache-tool"),
    };

    let blobs = base.join("blobs");
    let tmp = base.join("tmp");
    std::fs::create_dir_all(&blobs)?;
    std::fs::create_dir_all(&tmp)?;

    Ok((blobs.join(hash), tmp.join(Uuid::new_v4().to_string())))
}

fn install(tmp: &Path, final_path: &Path) -> std::io::Result<()> {
    if final_path.exists() {
        return std::fs::remove_file(tmp);
    }
    std::fs::rename(tmp, final_path)
}

fn pack(outputs: &[String], dest: &Path) -> std::io::Result<()> {
    let file = std::fs::File::create(dest)?;
    let encoder = zstd::Encoder::new(file, 3)?;
    let mut archive = tar::Builder::new(encoder);

    for out in outputs {
        let path = Path::new(out);
        if path.is_dir() {
            archive.append_dir_all(path, path)?;
        } else {
            archive.append_path(path)?;
        }
    }

    let encoder = archive.into_inner()?;
    let file = encoder.finish()?;
    file.sync_all()
}

fn unpack(src: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(src)?;
    let decoder = zstd::Decoder::new(file)?;
    tar::Archive::new(decoder).unpack(".")
}

async fn unpack_blocking(blob: &Path) -> Result<(), Error> {
    let blob = blob.to_path_buf();
    tokio::task::spawn_blocking(move || unpack(&blob)).await??;
    Ok(())
}

#[derive(Clone)]
struct AuthInterceptor(Option<String>);

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(key) = &self.0 {
            let value: MetadataValue<Ascii> = format!("Bearer {key}")
                .parse()
                .map_err(|_| tonic::Status::invalid_argument("API key is not valid ASCII"))?;
            request.metadata_mut().insert("authorization", value);
        }
        Ok(request)
    }
}

type Client = CacheClient<InterceptedService<Channel, AuthInterceptor>>;

fn brief(error: &Error) -> String {
    match error.downcast_ref::<tonic::Status>() {
        Some(status) => status.message().to_string(),
        None => error.to_string(),
    }
}

async fn connect(
    remote: &str,
    api_key: Option<String>,
    ca_cert: Option<&Path>,
) -> Result<Client, Error> {
    let mut endpoint = Channel::from_shared(remote.to_string())?;

    if remote.starts_with("https://") {
        let tls = match ca_cert {
            Some(path) => {
                ClientTlsConfig::new().ca_certificate(Certificate::from_pem(std::fs::read(path)?))
            }
            None => ClientTlsConfig::new().with_native_roots(),
        };
        endpoint = endpoint.tls_config(tls)?;
    }

    let channel = endpoint.connect().await?;
    Ok(CacheClient::with_interceptor(
        channel,
        AuthInterceptor(api_key),
    ))
}

/// Downloads the blob if the server has it. Ok(false) means a clean miss.
async fn fetch_remote(
    remote: &str,
    api_key: Option<String>,
    ca_cert: Option<&Path>,
    hash: &str,
    blob: &Path,
    tmp: &Path,
) -> Result<bool, Error> {
    let mut client = connect(remote, api_key, ca_cert).await?;

    let exists = client
        .check(CheckRequest {
            hash: hash.to_string(),
        })
        .await?
        .into_inner()
        .exists;

    if !exists {
        return Ok(false);
    }

    let mut stream = client
        .get(GetRequest {
            hash: hash.to_string(),
        })
        .await?
        .into_inner();

    let mut file = tokio::fs::File::create(tmp).await?;
    while let Some(response) = stream.message().await? {
        file.write_all(&response.chunk).await?;
    }
    file.sync_all().await?;
    drop(file);

    install(tmp, blob)?;
    Ok(true)
}

async fn upload_remote(
    remote: &str,
    api_key: Option<String>,
    ca_cert: Option<&Path>,
    hash: &str,
    blob: &Path,
) -> Result<(), Error> {
    let mut client = connect(remote, api_key, ca_cert).await?;

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let hash = hash.to_string();
    let blob = blob.to_path_buf();

    tokio::spawn(async move {
        let header = PutRequest {
            data: Some(put_request::Data::Hash(hash)),
        };
        if tx.send(header).await.is_err() {
            return;
        }

        let Ok(mut file) = tokio::fs::File::open(&blob).await else {
            return;
        };

        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            match file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let message = PutRequest {
                        data: Some(put_request::Data::Chunk(buf[..n].to_vec())),
                    };
                    if tx.send(message).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    client.put(ReceiverStream::new(rx)).await?;
    Ok(())
}

async fn run() -> Result<i32, Error> {
    let args = Args::parse();

    if args.api_key.is_some()
        && args
            .remote
            .as_deref()
            .is_some_and(|remote| remote.starts_with("http://"))
    {
        eprintln!("cache-tool: warning — sending an API key over plaintext HTTP");
    }

    if args.ca_cert.is_some()
        && !args
            .remote
            .as_deref()
            .is_some_and(|remote| remote.starts_with("https://"))
    {
        eprintln!("cache-tool: warning — --ca-cert has no effect without an https:// --remote");
    }

    let hash = compute_hash(&args, Path::new("."))?;
    let short = &hash[..12];
    let (blob_path, tmp_path) = cache_paths(&hash, args.cache_dir.as_deref())?;

    if blob_path.exists() {
        eprintln!("cache-tool: local hit {short}");
        unpack_blocking(&blob_path).await?;
        return Ok(0);
    }

    if let Some(remote) = &args.remote {
        match fetch_remote(
            remote,
            args.api_key.clone(),
            args.ca_cert.as_deref(),
            &hash,
            &blob_path,
            &tmp_path,
        )
        .await
        {
            Ok(true) => {
                eprintln!("cache-tool: remote hit {short}");
                unpack_blocking(&blob_path).await?;
                return Ok(0);
            }
            Ok(false) => {}
            Err(e) => eprintln!(
                "cache-tool: remote unavailable ({}), building locally",
                brief(&e)
            ),
        }
    }

    eprintln!("cache-tool: miss {short}, building");
    let status = std::process::Command::new(&args.command[0])
        .args(&args.command[1..])
        .status()?;

    if !status.success() {
        return Ok(status.code().unwrap_or(1));
    }

    let outputs = args.outputs.clone();
    let pack_dest = tmp_path.clone();
    let packed = tokio::task::spawn_blocking(move || pack(&outputs, &pack_dest)).await?;

    if let Err(e) = packed {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("could not archive outputs: {e}").into());
    }
    install(&tmp_path, &blob_path)?;

    if let Some(remote) = &args.remote
        && let Err(e) = upload_remote(
            remote,
            args.api_key.clone(),
            args.ca_cert.as_deref(),
            &hash,
            &blob_path,
        )
        .await
    {
        eprintln!("cache-tool: upload failed ({})", brief(&e));
    }

    Ok(0)
}

#[tokio::main]
async fn main() {
    match run().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("cache-tool: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: [&str; 2] = ["cargo", "build"];
    const OUT: [&str; 1] = ["out.bin"];

    fn args(outputs: &[&str], command: &[&str]) -> Args {
        Args {
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            remote: None,
            api_key: None,
            cache_dir: None,
            ca_cert: None,
            command: command.iter().map(|s| s.to_string()).collect(),
        }
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir().join(format!("cache-tool-fixture-{}", Uuid::new_v4()));
            let fixture = Fixture(root);
            for (relative, contents) in files {
                fixture.write(relative, contents);
            }
            fixture
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create fixture dir");
            }
            std::fs::write(&path, contents).expect("write fixture file");
        }

        fn hash(&self, outputs: &[&str], command: &[&str]) -> String {
            compute_hash(&args(outputs, command), &self.0).expect("compute hash")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ------------------------------------------------------ Determinism

    #[test]
    fn the_same_tree_hashes_the_same_way_twice() {
        let tree = Fixture::new(&[("src/a.rs", "one"), ("src/b.rs", "two")]);

        assert_eq!(tree.hash(&OUT, &CMD), tree.hash(&OUT, &CMD));
    }

    #[test]
    fn two_identical_trees_in_different_directories_hash_alike() {
        let files = [("src/a.rs", "one"), ("src/nested/b.rs", "two")];
        let first = Fixture::new(&files);
        let second = Fixture::new(&files);

        assert_ne!(first.path(), second.path());
        assert_eq!(first.hash(&OUT, &CMD), second.hash(&OUT, &CMD));
    }

    #[test]
    fn file_order_does_not_affect_the_hash() {
        let forwards = Fixture::new(&[("a.rs", "1"), ("b.rs", "2"), ("c.rs", "3")]);
        let backwards = Fixture::new(&[("c.rs", "3"), ("b.rs", "2"), ("a.rs", "1")]);

        assert_eq!(forwards.hash(&OUT, &CMD), backwards.hash(&OUT, &CMD));
    }

    // -------------------------------------------- What must change the key

    #[test]
    fn renaming_a_file_changes_the_hash() {
        let before = Fixture::new(&[("src/a.rs", "identical contents")]);
        let after = Fixture::new(&[("src/b.rs", "identical contents")]);

        assert_ne!(before.hash(&OUT, &CMD), after.hash(&OUT, &CMD));
    }

    #[test]
    fn moving_bytes_between_files_changes_the_hash() {
        // Both trees concatenate to the same bytes ("ab" + "c" and "a" + "bc"),
        // so only the length delimiter distinguishes them. A single file each:
        // with two files the interleaved filenames would disambiguate on their
        // own and the length would not actually be under test.
        let name_ab = Fixture::new(&[("ab", "c")]);
        let name_a = Fixture::new(&[("a", "bc")]);

        assert_ne!(name_ab.hash(&OUT, &CMD), name_a.hash(&OUT, &CMD));
    }

    #[test]
    fn inputs_are_collected_in_sorted_order() {
        // Tested directly rather than through the hash: NTFS already returns
        // directory entries in name order, so an end-to-end test cannot observe
        // a missing sort on Windows. It is still load-bearing on ext4 and on
        // network filesystems.
        let tree = Fixture::new(&[
            ("zebra.rs", "z"),
            ("alpha.rs", "a"),
            ("middle/m.rs", "m"),
            ("beta.rs", "b"),
        ]);

        let collected = collect_inputs(&args(&OUT, &CMD), tree.path());
        let paths: Vec<&str> = collected.iter().map(|(p, _)| p.as_str()).collect();

        let mut expected = paths.clone();
        expected.sort_unstable();

        assert_eq!(paths, expected, "collect_inputs must return sorted paths");
        assert_eq!(paths.len(), 4);
    }

    #[test]
    fn changing_the_build_command_changes_the_hash() {
        let tree = Fixture::new(&[("src/a.rs", "source")]);

        assert_ne!(
            tree.hash(&OUT, &["cargo", "build"]),
            tree.hash(&OUT, &["cargo", "build", "--release"])
        );
    }

    #[test]
    fn changing_the_declared_outputs_changes_the_hash() {
        let tree = Fixture::new(&[("src/a.rs", "source")]);

        assert_ne!(tree.hash(&["a.bin"], &CMD), tree.hash(&["b.bin"], &CMD));
    }

    #[test]
    fn editing_a_file_changes_the_hash() {
        let tree = Fixture::new(&[("src/a.rs", "before")]);
        let before = tree.hash(&OUT, &CMD);

        tree.write("src/a.rs", "after");

        assert_ne!(before, tree.hash(&OUT, &CMD));
    }

    // ---------------------------------------- What must NOT change the key

    #[test]
    fn declared_outputs_are_not_hashed_as_inputs() {
        let tree = Fixture::new(&[("src/a.rs", "source"), ("out.bin", "first build")]);
        let before = tree.hash(&["out.bin"], &CMD);

        tree.write("out.bin", "a completely different artifact");

        assert_eq!(
            before,
            tree.hash(&["out.bin"], &CMD),
            "hashing a declared output makes the key change every build"
        );
    }

    #[test]
    fn a_declared_output_directory_is_excluded_recursively() {
        let tree = Fixture::new(&[("src/a.rs", "source"), ("dist/app.js", "first")]);
        let before = tree.hash(&["dist"], &CMD);

        tree.write("dist/app.js", "rebuilt");
        tree.write("dist/nested/chunk.js", "new file");

        assert_eq!(before, tree.hash(&["dist"], &CMD));
    }

    #[test]
    fn ignored_directories_do_not_affect_the_hash() {
        let tree = Fixture::new(&[("src/a.rs", "source")]);
        let before = tree.hash(&OUT, &CMD);

        tree.write(".git/HEAD", "ref: refs/heads/master");
        tree.write("target/debug/app", "compiled");
        tree.write("node_modules/pkg/index.js", "dependency");
        tree.write("cache-storage/blobs/abc", "someone else's blob");

        assert_eq!(before, tree.hash(&OUT, &CMD));
    }

    // ------------------------------------------------------ Normalisation

    #[test]
    fn backslashes_and_forward_slashes_normalize_alike() {
        assert_eq!(normalize(Path::new(r"src\main.rs")), "src/main.rs");
        assert_eq!(normalize(Path::new("src/main.rs")), "src/main.rs");
        assert_eq!(normalize(Path::new("./src/main.rs")), "src/main.rs");
        assert_eq!(
            normalize(Path::new(r".\src\nested\a.rs")),
            "src/nested/a.rs"
        );
    }

    // ------------------------------------------------------------- Errors

    #[test]
    fn an_unreadable_file_is_an_error_rather_than_an_empty_hash() {
        let tree = Fixture::new(&[("src/a.rs", "readable")]);
        let blocked = tree.path().join("blocked.rs");
        std::fs::write(&blocked, "unreadable").expect("write blocked file");

        #[cfg(windows)]
        let _exclusive_handle = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&blocked)
                .expect("open with exclusive share mode")
        };

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000))
                .expect("chmod 000");

            if std::fs::read(&blocked).is_ok() {
                // Running with privileges that ignore mode bits, e.g. root in a
                // container. The property cannot be exercised here.
                return;
            }
        }

        let result = compute_hash(&args(&OUT, &CMD), tree.path());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644));
        }

        assert!(
            result.is_err(),
            "an unreadable file must fail the hash rather than be treated as empty"
        );
    }
}
