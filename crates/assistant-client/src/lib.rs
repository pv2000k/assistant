use assistant_protocol::{
    PROTOCOL_VERSION, RequestMethod, ResponsePayload, WireRequest, WireResponse,
};
use std::{
    env,
    error::Error,
    fmt,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const LEASE_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

pub struct IpcClient {
    socket_path: PathBuf,
    timeout: Duration,
    next_id: AtomicU64,
}

pub fn default_socket_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = env::var_os("ASSISTANT_SOCKET_PATH") {
        return Ok(PathBuf::from(value));
    }

    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime_dir).join("assistant.sock"));
    }

    let home = env::var_os("HOME").ok_or("HOME environment variable is not set.")?;
    Ok(PathBuf::from(home).join(".cache/assistant/assistant.sock"))
}

impl IpcClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: DEFAULT_TIMEOUT,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn request(&self, method: RequestMethod) -> Result<ResponsePayload, IpcClientError> {
        let response = self.request_wire(method)?;

        if let Some(error) = response.error {
            return Err(IpcClientError::Server {
                code: error.code,
                message: error.message,
            });
        }

        response.result.ok_or(IpcClientError::MissingResult)
    }

    pub fn acquire_lease(
        &self,
        client_id: &str,
    ) -> Result<assistant_protocol::LeaseStatus, IpcClientError> {
        self.expect_lease(RequestMethod::ClientAcquire {
            client_id: client_id.to_string(),
        })
    }

    pub fn heartbeat_lease(
        &self,
        client_id: &str,
    ) -> Result<assistant_protocol::LeaseStatus, IpcClientError> {
        self.expect_lease(RequestMethod::ClientHeartbeat {
            client_id: client_id.to_string(),
        })
    }

    pub fn release_lease(
        &self,
        client_id: &str,
    ) -> Result<assistant_protocol::LeaseStatus, IpcClientError> {
        self.expect_lease(RequestMethod::ClientRelease {
            client_id: client_id.to_string(),
        })
    }

    fn expect_lease(
        &self,
        method: RequestMethod,
    ) -> Result<assistant_protocol::LeaseStatus, IpcClientError> {
        match self.request(method)? {
            ResponsePayload::Lease(status) => Ok(status),
            other => Err(IpcClientError::Server {
                code: "unexpected_response".to_string(),
                message: format!("Expected lease response, received {other:?}."),
            }),
        }
    }

    pub fn request_wire(&self, method: RequestMethod) -> Result<WireResponse, IpcClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = WireRequest::new(id, method);
        let expected_id = request.id;

        let mut stream = UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let request_line = request.encode_line().map_err(IpcClientError::Encode)?;
        stream.write_all(request_line.as_bytes())?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        let bytes_read = reader.read_line(&mut response_line)?;
        if bytes_read == 0 {
            return Err(IpcClientError::ConnectionClosed);
        }

        let response = WireResponse::decode_line(&response_line).map_err(IpcClientError::Decode)?;

        if response.id != expected_id {
            return Err(IpcClientError::ResponseIdMismatch {
                expected: expected_id,
                actual: response.id,
            });
        }

        if response.version != PROTOCOL_VERSION {
            return Err(IpcClientError::ProtocolVersionMismatch {
                expected: PROTOCOL_VERSION,
                actual: response.version,
            });
        }

        Ok(response)
    }
}

pub struct ClientLease {
    socket_path: PathBuf,
    client_id: String,
    stop: Arc<AtomicBool>,
    heartbeat: Option<JoinHandle<()>>,
}

impl ClientLease {
    pub fn acquire(socket_path: impl Into<PathBuf>) -> Result<Self, IpcClientError> {
        let socket_path = socket_path.into();
        let client_id = format!(
            "client-{}-{}",
            std::process::id(),
            NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed)
        );

        let client = IpcClient::new(&socket_path).with_timeout(LEASE_REQUEST_TIMEOUT);
        client.acquire_lease(&client_id)?;

        let stop = Arc::new(AtomicBool::new(false));
        let heartbeat_stop = Arc::clone(&stop);
        let heartbeat_socket = socket_path.clone();
        let heartbeat_id = client_id.clone();
        let heartbeat = match thread::Builder::new()
            .name("assistant-ipc-lease-heartbeat".to_string())
            .spawn(move || {
                let client = IpcClient::new(&heartbeat_socket).with_timeout(LEASE_REQUEST_TIMEOUT);
                while !heartbeat_stop.load(Ordering::Relaxed) {
                    let slices = LEASE_HEARTBEAT_INTERVAL.as_millis().div_ceil(100).max(1);
                    for _ in 0..slices {
                        if heartbeat_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }

                    if heartbeat_stop.load(Ordering::Relaxed) {
                        return;
                    }

                    let _ = client.heartbeat_lease(&heartbeat_id);
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = client.release_lease(&client_id);
                return Err(IpcClientError::Io(error));
            }
        };

        Ok(Self {
            socket_path,
            client_id,
            stop,
            heartbeat: Some(heartbeat),
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }
}

impl Drop for ClientLease {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.heartbeat.take() {
            let _ = handle.join();
        }

        let client = IpcClient::new(&self.socket_path).with_timeout(LEASE_REQUEST_TIMEOUT);
        let _ = client.release_lease(&self.client_id);
    }
}

#[derive(Debug)]
pub enum IpcClientError {
    Io(std::io::Error),
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    Server { code: String, message: String },
    ConnectionClosed,
    ResponseIdMismatch { expected: u64, actual: u64 },
    ProtocolVersionMismatch { expected: u32, actual: u32 },
    MissingResult,
}

impl fmt::Display for IpcClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IPC I/O error: {error}"),
            Self::Encode(error) => write!(f, "Failed to encode IPC request: {error}"),
            Self::Decode(error) => write!(f, "Failed to decode IPC response: {error}"),
            Self::Server { code, message } => write!(f, "IPC server error ({code}): {message}"),
            Self::ConnectionClosed => {
                write!(f, "IPC connection closed before a response was received.")
            }
            Self::ResponseIdMismatch { expected, actual } => write!(
                f,
                "IPC response id mismatch: expected {expected}, received {actual}."
            ),
            Self::ProtocolVersionMismatch { expected, actual } => write!(
                f,
                "IPC protocol version mismatch: expected {expected}, received {actual}."
            ),
            Self::MissingResult => {
                write!(f, "IPC response did not contain a result or error.")
            }
        }
    }
}

impl Error for IpcClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for IpcClientError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_protocol::HealthStatus;
    use std::{
        io::BufReader,
        os::unix::net::UnixListener,
        path::{Path, PathBuf},
        thread,
    };

    fn test_socket_path() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("assistant.sock");
        (directory, path)
    }

    fn spawn_server(
        socket_path: &Path,
        response: WireResponse,
    ) -> thread::JoinHandle<Result<(), String>> {
        let listener = UnixListener::bind(socket_path).expect("bind test socket");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let mut request_line = String::new();
            BufReader::new(stream.try_clone().map_err(|error| error.to_string())?)
                .read_line(&mut request_line)
                .map_err(|error| error.to_string())?;
            WireRequest::decode_line(&request_line).map_err(|error| error.to_string())?;
            stream
                .write_all(
                    response
                        .encode_line()
                        .map_err(|error| error.to_string())?
                        .as_bytes(),
                )
                .map_err(|error| error.to_string())?;
            stream.flush().map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    #[test]
    fn request_round_trip_returns_decoded_payload() {
        let (_directory, socket_path) = test_socket_path();
        let server = spawn_server(&socket_path, WireResponse::ok(1, ResponsePayload::Pong));

        let client = IpcClient::new(&socket_path);
        let result = client
            .request(RequestMethod::Ping)
            .expect("IPC request succeeds");

        assert_eq!(result, ResponsePayload::Pong);
        server
            .join()
            .expect("server thread joins")
            .expect("server succeeds");
    }

    #[test]
    fn server_errors_are_decoded_into_client_errors() {
        let (_directory, socket_path) = test_socket_path();
        let server = spawn_server(
            &socket_path,
            WireResponse::error(1, "unsupported", "This method is not available."),
        );

        let client = IpcClient::new(&socket_path);
        let error = client
            .request(RequestMethod::Ping)
            .expect_err("server error should be returned");

        assert!(matches!(
            error,
            IpcClientError::Server { code, message }
                if code == "unsupported" && message == "This method is not available."
        ));
        server
            .join()
            .expect("server thread joins")
            .expect("server succeeds");
    }

    #[test]
    fn response_id_mismatch_is_rejected() {
        let (_directory, socket_path) = test_socket_path();
        let server = spawn_server(&socket_path, WireResponse::ok(999, ResponsePayload::Pong));

        let client = IpcClient::new(&socket_path);
        let error = client
            .request(RequestMethod::Ping)
            .expect_err("response id mismatch should fail");

        assert!(matches!(
            error,
            IpcClientError::ResponseIdMismatch {
                expected: 1,
                actual: 999
            }
        ));
        server
            .join()
            .expect("server thread joins")
            .expect("server succeeds");
    }

    #[test]
    fn protocol_version_mismatch_is_rejected() {
        let (_directory, socket_path) = test_socket_path();
        let mut response = WireResponse::ok(
            1,
            ResponsePayload::Health(HealthStatus {
                runtime: "test".to_string(),
                ready: true,
            }),
        );
        response.version = PROTOCOL_VERSION + 1;
        let server = spawn_server(&socket_path, response);

        let client = IpcClient::new(&socket_path);
        let error = client
            .request(RequestMethod::Health)
            .expect_err("protocol version mismatch should fail");

        assert!(matches!(
            error,
            IpcClientError::ProtocolVersionMismatch {
                expected: PROTOCOL_VERSION,
                actual
            } if actual == PROTOCOL_VERSION + 1
        ));
        server
            .join()
            .expect("server thread joins")
            .expect("server succeeds");
    }

    #[test]
    fn client_lease_acquires_and_releases_over_ipc() {
        let (_directory, socket_path) = test_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let socket_for_thread = socket_path.clone();
        let server = thread::spawn(move || -> Result<(), String> {
            for expected_method in [
                RequestMethod::ClientAcquire {
                    client_id: String::new(),
                },
                RequestMethod::ClientRelease {
                    client_id: String::new(),
                },
            ] {
                let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
                let mut request_line = String::new();
                BufReader::new(stream.try_clone().map_err(|error| error.to_string())?)
                    .read_line(&mut request_line)
                    .map_err(|error| error.to_string())?;
                let request =
                    WireRequest::decode_line(&request_line).map_err(|error| error.to_string())?;
                match (expected_method, request.method) {
                    (
                        RequestMethod::ClientAcquire { .. },
                        RequestMethod::ClientAcquire { client_id },
                    )
                    | (
                        RequestMethod::ClientRelease { .. },
                        RequestMethod::ClientRelease { client_id },
                    ) => {
                        let response = WireResponse::ok(
                            request.id,
                            ResponsePayload::Lease(assistant_protocol::LeaseStatus {
                                client_id,
                                active_clients: 1,
                            }),
                        );
                        stream
                            .write_all(
                                response
                                    .encode_line()
                                    .map_err(|error| error.to_string())?
                                    .as_bytes(),
                            )
                            .map_err(|error| error.to_string())?;
                        stream.flush().map_err(|error| error.to_string())?;
                    }
                    (_, method) => {
                        return Err(format!("unexpected IPC lease method: {method:?}"));
                    }
                }
            }
            Ok(())
        });

        let lease = ClientLease::acquire(&socket_for_thread).expect("lease acquisition succeeds");
        assert!(
            lease
                .client_id()
                .starts_with(&format!("client-{}-", std::process::id()))
        );
        drop(lease);

        server
            .join()
            .expect("server thread joins")
            .expect("server succeeds");
    }

    #[test]
    fn client_lease_ids_are_non_empty_and_process_scoped() {
        let first = format!(
            "client-{}-{}",
            std::process::id(),
            NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let second = format!(
            "client-{}-{}",
            std::process::id(),
            NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed)
        );
        assert!(!first.is_empty());
        assert_ne!(first, second);
    }

    #[test]
    fn socket_path_accessor_returns_configured_path() {
        let (_directory, socket_path) = test_socket_path();
        let client = IpcClient::new(&socket_path);
        assert_eq!(client.socket_path(), socket_path.as_path());
    }
}
