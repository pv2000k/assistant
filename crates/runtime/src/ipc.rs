pub use assistant_client::default_socket_path;
use assistant_protocol::{WireRequest, WireResponse};
use std::{
    error::Error,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::FileTypeExt,
        net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};

pub trait RequestHandler: Send + Sync + 'static {
    fn handle(&self, request: WireRequest) -> WireResponse;

    fn should_shutdown(&self) -> bool {
        false
    }
}

pub fn serve<H>(socket_path: &Path, handler: Arc<H>) -> Result<(), Box<dyn Error + Send + Sync>>
where
    H: RequestHandler,
{
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if socket_path.exists() {
        let metadata = fs::symlink_metadata(socket_path)?;
        if !metadata.file_type().is_socket() {
            return Err(format!(
                "IPC socket path exists and is not a Unix socket: {}",
                socket_path.display()
            )
            .into());
        }
        fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    println!("IPC socket: {}", socket_path.display());
    println!("Runtime daemon is ready.");

    loop {
        if handler.should_shutdown() {
            println!("Runtime idle timeout reached; shutting down.");
            break;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, handler) {
                        eprintln!("IPC connection failed: {error}");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => eprintln!("IPC accept failed: {error}"),
        }
    }

    let _ = fs::remove_file(socket_path);
    Ok(())
}

fn handle_connection<H>(
    stream: UnixStream,
    handler: Arc<H>,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
    H: RequestHandler,
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();

    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }

    let response = match WireRequest::decode_line(&line) {
        Ok(request) => handler.handle(request),
        Err(error) => WireResponse::error(0, "invalid_request", error.to_string()),
    };

    let mut stream = stream;
    stream.write_all(response.encode_line()?.as_bytes())?;
    stream.flush()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_protocol::{RequestMethod, ResponsePayload};

    struct TestHandler;

    impl RequestHandler for TestHandler {
        fn handle(&self, request: WireRequest) -> WireResponse {
            match request.method {
                RequestMethod::Ping => WireResponse::ok(request.id, ResponsePayload::Pong),
                _ => WireResponse::error(request.id, "unsupported", "unsupported in test"),
            }
        }
    }

    struct ShutdownHandler;

    impl RequestHandler for ShutdownHandler {
        fn handle(&self, request: WireRequest) -> WireResponse {
            WireResponse::error(request.id, "unused", "unused")
        }

        fn should_shutdown(&self) -> bool {
            true
        }
    }

    #[test]
    fn serve_exits_when_handler_requests_shutdown() -> Result<(), Box<dyn Error + Send + Sync>> {
        let directory = tempfile::tempdir()?;
        let socket_path = directory.path().join("assistant.sock");
        let handler = Arc::new(ShutdownHandler);
        let socket_for_thread = socket_path.clone();
        let thread = std::thread::spawn(move || serve(&socket_for_thread, handler));

        thread.join().map_err(|_| "IPC server thread panicked.")??;
        assert!(!socket_path.exists());
        Ok(())
    }

    #[test]
    fn connection_round_trip_uses_protocol_lines() -> Result<(), Box<dyn Error + Send + Sync>> {
        let (mut client, server) = UnixStream::pair()?;
        let handler = Arc::new(TestHandler);

        let thread = std::thread::spawn(move || handle_connection(server, handler));

        let request = WireRequest::new(7, RequestMethod::Ping);
        client.write_all(request.encode_line()?.as_bytes())?;
        client.flush()?;

        let mut response_line = String::new();
        BufReader::new(client).read_line(&mut response_line)?;

        let response = WireResponse::decode_line(&response_line)?;
        assert_eq!(response.id, 7);
        assert_eq!(response.result, Some(ResponsePayload::Pong));
        assert!(response.error.is_none());

        thread.join().map_err(|_| "IPC test thread panicked.")??;

        Ok(())
    }
}
