use blake3::Hasher;
use clap::Parser;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;
use walkdir::WalkDir;

use tonic::codegen::InterceptedService;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::Channel;

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

async fn connect(remote: &str, api_key: Option<String>) -> Result<Client, Error> {
    let channel = Channel::from_shared(remote.to_string())?.connect().await?;
    Ok(CacheClient::with_interceptor(
        channel,
        AuthInterceptor(api_key),
    ))
}

/// Downloads the blob if the server has it. Ok(false) means a clean miss.
async fn fetch_remote(
    remote: &str,
    api_key: Option<String>,
    hash: &str,
    blob: &Path,
    tmp: &Path,
) -> Result<bool, Error> {
    let mut client = connect(remote, api_key).await?;

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
    hash: &str,
    blob: &Path,
) -> Result<(), Error> {
    let mut client = connect(remote, api_key).await?;

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

    let hash = compute_hash(&args, Path::new("."))?;
    let short = &hash[..12];
    let (blob_path, tmp_path) = cache_paths(&hash, args.cache_dir.as_deref())?;

    if blob_path.exists() {
        eprintln!("cache-tool: local hit {short}");
        unpack_blocking(&blob_path).await?;
        return Ok(0);
    }

    if let Some(remote) = &args.remote {
        match fetch_remote(remote, args.api_key.clone(), &hash, &blob_path, &tmp_path).await {
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
        && let Err(e) = upload_remote(remote, args.api_key.clone(), &hash, &blob_path).await
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
