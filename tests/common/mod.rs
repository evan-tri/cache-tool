#![allow(dead_code)]

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cache_tool::cache::cache_client::CacheClient;
use tonic::codegen::InterceptedService;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::transport::Channel;

pub type TestClient = CacheClient<InterceptedService<Channel, AuthInterceptor>>;

#[derive(Clone)]
pub struct AuthInterceptor(pub Option<String>);

impl Interceptor for AuthInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(key) = &self.0 {
            let value: MetadataValue<Ascii> = format!("Bearer {key}")
                .parse()
                .map_err(|_| tonic::Status::invalid_argument("bad key"))?;
            request.metadata_mut().insert("authorization", value);
        }
        Ok(request)
    }
}

pub struct TestServer {
    child: Option<Child>,
    api_key: Option<String>,
    max_size_gb: Option<f64>,
    pub port: u16,
    pub storage: PathBuf,
}

impl TestServer {
    pub fn start() -> Self {
        Self::start_with(None, None)
    }

    pub fn start_with(api_key: Option<&str>, max_size_gb: Option<f64>) -> Self {
        let port = free_port();
        let storage = std::env::temp_dir().join(format!("cache-tool-test-{port}-{}", now_nanos()));
        std::fs::create_dir_all(&storage).expect("create storage dir");

        let mut server = TestServer {
            child: None,
            api_key: api_key.map(str::to_owned),
            max_size_gb,
            port,
            storage,
        };
        server.start_again();
        server
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn start_again(&mut self) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cache-server"));
        command
            .arg("--addr")
            .arg(format!("127.0.0.1:{}", self.port))
            .arg("--storage")
            .arg(&self.storage)
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(key) = &self.api_key {
            command.arg("--api-key").arg(key);
        }
        if let Some(gb) = self.max_size_gb {
            command.arg("--max-size-gb").arg(format!("{gb:.12}"));
        }

        command.env_remove("CACHE_SERVER_API_KEY");

        self.child = Some(command.spawn().expect("spawn cache-server"));
        self.wait_until_listening();
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn blobs(&self) -> PathBuf {
        self.storage.join("blobs")
    }

    pub fn tmp(&self) -> PathBuf {
        self.storage.join("tmp")
    }

    pub fn blob_count(&self) -> usize {
        count_entries(&self.blobs())
    }

    pub fn tmp_count(&self) -> usize {
        count_entries(&self.tmp())
    }

    pub fn has_blob(&self, hash: &str) -> bool {
        self.blobs().join(hash).exists()
    }

    pub fn blob_bytes(&self, hash: &str) -> Vec<u8> {
        std::fs::read(self.blobs().join(hash)).expect("read blob")
    }

    pub fn delete_index(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(self.storage.join(format!("metadata.db{suffix}")));
        }
    }

    pub async fn client(&self) -> TestClient {
        self.client_with_key(None).await
    }

    pub async fn client_with_key(&self, api_key: Option<&str>) -> TestClient {
        let channel = Channel::from_shared(self.url())
            .expect("valid url")
            .connect()
            .await
            .expect("connect to test server");

        CacheClient::with_interceptor(channel, AuthInterceptor(api_key.map(str::to_owned)))
    }

    fn wait_until_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("cache-server did not start listening on port {}", self.port);
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.storage);
    }
}

pub fn test_hash(label: &str) -> String {
    blake3::hash(label.as_bytes()).to_hex().to_string()
}

/// Converts a byte budget into the --max-size-gb value the server expects.
pub fn gb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn count_entries(dir: &PathBuf) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0)
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("read local addr")
        .port()
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
