use reqwest::blocking::Client;
use serde_json::Value;
use std::{
    env,
    error::Error,
    io,
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_MODEL_ROOT: &str = "AI/models";
const DEFAULT_EMBEDDING_ROOT: &str = "AI/embeddings";
const DEFAULT_LLAMA_ROOT: &str = "dev/local-ai";
const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const SERVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(90);
const SERVICE_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct LocalServiceSupervisor {
    generation: ManagedProcess,
    generation_url: String,
    generation_model: String,
    embedding: ManagedProcess,
}

impl LocalServiceSupervisor {
    pub fn start(
        generation_url: &str,
        generation_model: &str,
        embedding_url: &str,
        embedding_model: &str,
    ) -> Result<Self, Box<dyn Error>> {
        let llama_root = llama_root()?;
        let generation_path = resolve_model_path(&model_root()?, generation_model, "generation")?;
        let embedding_path = resolve_embedding_model_path(embedding_model)?;

        let generation = ensure_generation_service(
            generation_url,
            generation_model,
            &generation_path,
            &llama_root,
        )?;
        let embedding =
            ensure_embedding_service(embedding_url, embedding_model, &embedding_path, &llama_root)?;

        Ok(Self {
            generation,
            generation_url: generation_url.to_string(),
            generation_model: generation_model.to_string(),
            embedding,
        })
    }

    pub fn switch_generation_model(&mut self, model: &str) -> Result<(), Box<dyn Error>> {
        validate_generation_model_name(model)?;
        if model == self.generation_model {
            return Ok(());
        }

        if !self.generation.is_owned() {
            return Err(
                "The assistant does not own the current generation process, so it cannot switch models in place."
                    .into(),
            );
        }

        let old_model = self.generation_model.clone();
        let model_path = resolve_model_path(&model_root()?, model, "generation")?;

        self.generation.stop();

        match ensure_generation_service(&self.generation_url, model, &model_path, &llama_root()?) {
            Ok(generation) => {
                self.generation = generation;
                self.generation_model = model.to_string();
                Ok(())
            }
            Err(error) => {
                let old_path = resolve_model_path(&model_root()?, &old_model, "generation");
                match old_path {
                    Ok(old_path) => match ensure_generation_service(
                        &self.generation_url,
                        &old_model,
                        &old_path,
                        &llama_root()?,
                    ) {
                        Ok(generation) => {
                            self.generation = generation;
                            self.generation_model = old_model.clone();
                            Err(format!(
                                "Could not switch generation model to '{model}': {error}. Restored '{old_model}'."
                            )
                            .into())
                        }
                        Err(recovery_error) => Err(format!(
                            "Could not switch generation model to '{model}': {error}. Could not restore '{old_model}': {recovery_error}"
                        )
                        .into()),
                    },
                    Err(recovery_error) => Err(format!(
                        "Could not switch generation model to '{model}': {error}. Could not resolve previous model '{old_model}': {recovery_error}"
                    )
                    .into()),
                }
            }
        }
    }
}

impl Drop for LocalServiceSupervisor {
    fn drop(&mut self) {
        self.generation.stop();
        self.embedding.stop();
    }
}

#[derive(Debug)]
struct ManagedProcess {
    name: String,
    child: Option<Child>,
}

impl ManagedProcess {
    fn reused(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            child: None,
        }
    }

    fn owned(name: impl Into<String>, child: Child) -> Self {
        Self {
            name: name.into(),
            child: Some(child),
        }
    }

    fn is_owned(&self) -> bool {
        self.child.is_some()
    }

    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        if let Ok(None) = child.try_wait() {
            println!("Stopping {}...", self.name);
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

fn ensure_generation_service(
    url: &str,
    model: &str,
    model_path: &Path,
    llama_root: &Path,
) -> Result<ManagedProcess, Box<dyn Error>> {
    if let Some(served) = ready_model(url)? {
        if served == model {
            println!("Local generation model already running: {model}");
            return Ok(ManagedProcess::reused("generation model"));
        }

        return Err(format!(
            "Generation endpoint {} is already serving '{}', not '{}'. Refusing to replace a process the assistant does not own.",
            url, served, model
        )
        .into());
    }

    refuse_if_port_is_occupied(url, "generation")?;

    println!("Starting local generation model: {model}");
    let child = spawn_llama_server(
        "generation model",
        model_path,
        url,
        &["-c", "4096", "-np", "1", "--reasoning", "off"],
        llama_root,
    )?;

    wait_for_model(url, model, model_path, child, "generation")
}

fn ensure_embedding_service(
    url: &str,
    model: &str,
    model_path: &Path,
    llama_root: &Path,
) -> Result<ManagedProcess, Box<dyn Error>> {
    if let Some(served) = ready_model(url)? {
        if served == model {
            println!("Embedding model already running: {model}");
            return Ok(ManagedProcess::reused("embedding model"));
        }

        return Err(format!(
            "Embedding endpoint {} is already serving '{}', not '{}'. Refusing to replace a process the assistant does not own.",
            url, served, model
        )
        .into());
    }

    refuse_if_port_is_occupied(url, "embedding")?;

    println!("Starting embedding model: {model}");
    let child = spawn_llama_server(
        "embedding model",
        model_path,
        url,
        &["--embedding", "--pooling", "cls", "-np", "1", "-c", "512"],
        llama_root,
    )?;

    wait_for_model(url, model, model_path, child, "embedding")
}

fn spawn_llama_server(
    name: &str,
    model_path: &Path,
    url: &str,
    extra_args: &[&str],
    llama_root: &Path,
) -> Result<Child, Box<dyn Error>> {
    let (host, port) = parse_http_endpoint(url)?;

    let mut command = Command::new("nix");
    command
        .arg("develop")
        .arg(llama_root)
        .arg("-c")
        .arg("llama-server")
        .arg("-m")
        .arg(model_path)
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port);

    for argument in extra_args {
        command.arg(argument);
    }

    command.stdin(Stdio::null());

    command
        .spawn()
        .map_err(|error| format!("Could not start {name} through nix develop: {error}").into())
}

fn wait_for_model(
    url: &str,
    expected_model: &str,
    model_path: &Path,
    mut child: Child,
    kind: &str,
) -> Result<ManagedProcess, Box<dyn Error>> {
    let deadline = Instant::now() + SERVICE_START_TIMEOUT;

    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!(
                "Local {kind} process exited before {} became ready. Model: {}. Process status: {}",
                url,
                model_path.display(),
                status
            )
            .into());
        }

        if ready_model(url)?.as_deref() == Some(expected_model) {
            println!("Local {kind} model ready: {expected_model} at {url}");
            return Ok(ManagedProcess::owned(format!("{kind} model"), child));
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "Timed out waiting for local {kind} model '{}' at {} to become ready.",
                expected_model, url
            )
            .into());
        }

        thread::sleep(SERVICE_POLL_INTERVAL);
    }
}

fn ready_model(url: &str) -> Result<Option<String>, Box<dyn Error>> {
    let endpoint = format!("{}/v1/models", url.trim_end_matches('/'));
    let client = Client::builder()
        .connect_timeout(SERVICE_CONNECT_TIMEOUT)
        .timeout(SERVICE_REQUEST_TIMEOUT)
        .build()?;

    let response = match client.get(endpoint).send() {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };

    if !response.status().is_success() {
        return Ok(None);
    }

    let body: Value = response.json()?;
    let models = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.get("models").and_then(Value::as_array));

    Ok(models.and_then(|values| {
        values.iter().find_map(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .or_else(|| item.get("model").and_then(Value::as_str))
                .or_else(|| item.get("name").and_then(Value::as_str))
                .map(str::to_string)
        })
    }))
}

fn refuse_if_port_is_occupied(url: &str, kind: &str) -> Result<(), Box<dyn Error>> {
    let (host, port) = parse_http_endpoint(url)?;
    let address = format!("{host}:{port}");
    let address: SocketAddr = address.parse().map_err(|error| {
        format!("Could not parse local {kind} endpoint address {address}: {error}")
    })?;

    match TcpStream::connect_timeout(&address, SERVICE_CONNECT_TIMEOUT) {
        Ok(_) => Err(format!(
            "Local {kind} endpoint {} is occupied but is not serving a compatible llama.cpp model. Refusing to replace an unmanaged process.",
            url
        )
        .into()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
            ) => Ok(()),
        Err(error) => Err(format!(
            "Could not determine whether {kind} endpoint {} is available: {error}",
            url
        )
        .into()),
    }
}

fn validate_generation_model_name(model: &str) -> Result<(), Box<dyn Error>> {
    let model = model.trim();
    let path = Path::new(model);
    if model.is_empty()
        || path.file_name().and_then(|value| value.to_str()) != Some(model)
        || path
            .extension()
            .and_then(|value| value.to_str())
            .is_none_or(|value| !value.eq_ignore_ascii_case("gguf"))
    {
        return Err(format!("Generation model must be a single .gguf filename: {model}").into());
    }

    Ok(())
}

fn resolve_model_path(root: &Path, model: &str, kind: &str) -> Result<PathBuf, Box<dyn Error>> {
    let candidate = PathBuf::from(model);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        root.join(model)
    };

    if !path.is_file() {
        return Err(format!(
            "Configured {kind} model '{}' was not found at {}.",
            model,
            path.display()
        )
        .into());
    }

    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_none_or(|value| !value.eq_ignore_ascii_case("gguf"))
    {
        return Err(format!(
            "Configured {kind} model must be a .gguf file: {}",
            path.display()
        )
        .into());
    }

    Ok(path)
}

fn resolve_embedding_model_path(model: &str) -> Result<PathBuf, Box<dyn Error>> {
    let embedding_root = configured_root("ASSISTANT_EMBEDDING_ROOT", DEFAULT_EMBEDDING_ROOT)?;
    if let Ok(path) = resolve_model_path(&embedding_root, model, "embedding") {
        return Ok(path);
    }

    let fallback_root = model_root()?;
    resolve_model_path(&fallback_root, model, "embedding")
}

fn model_root() -> Result<PathBuf, Box<dyn Error>> {
    let root = configured_root("ASSISTANT_MODEL_ROOT", DEFAULT_MODEL_ROOT)?;
    ensure_directory("ASSISTANT_MODEL_ROOT", &root)?;
    Ok(root)
}

fn llama_root() -> Result<PathBuf, Box<dyn Error>> {
    let root = configured_root("ASSISTANT_LLAMA_ROOT", DEFAULT_LLAMA_ROOT)?;
    ensure_directory("ASSISTANT_LLAMA_ROOT", &root)?;
    Ok(root)
}

fn configured_root(variable: &str, default_suffix: &str) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = env::var_os(variable) {
        return Ok(PathBuf::from(value));
    }

    let home = env::var_os("HOME").ok_or("HOME environment variable is not set.")?;
    Ok(PathBuf::from(home).join(default_suffix))
}

fn ensure_directory(variable: &str, path: &Path) -> Result<(), Box<dyn Error>> {
    if !path.is_dir() {
        return Err(format!(
            "{variable} does not point to a directory: {}",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn parse_http_endpoint(url: &str) -> Result<(String, String), Box<dyn Error>> {
    let url = url.trim_end_matches('/');
    let stripped = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("Local model URL must use http://: {url}"))?;

    let (host, port) = stripped
        .split_once(':')
        .ok_or_else(|| format!("Local model URL must contain host and port: {url}"))?;

    if host.is_empty() || port.is_empty() || port.contains('/') {
        return Err(format!("Invalid local model URL: {url}").into());
    }

    Ok((host.to_string(), port.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_http_endpoint() {
        assert_eq!(
            parse_http_endpoint("http://127.0.0.1:8080").unwrap(),
            ("127.0.0.1".to_string(), "8080".to_string())
        );
    }

    #[test]
    fn rejects_non_http_endpoint() {
        assert!(parse_http_endpoint("https://127.0.0.1:8080").is_err());
    }

    #[test]
    fn rejects_path_suffix() {
        assert!(parse_http_endpoint("http://127.0.0.1:8080/base").is_err());
    }

    #[test]
    fn generation_model_name_rejects_path_traversal() {
        assert!(validate_generation_model_name("../other.gguf").is_err());
        assert!(validate_generation_model_name("/tmp/other.gguf").is_err());
        assert!(validate_generation_model_name("other.gguf").is_ok());
        assert!(validate_generation_model_name("OTHER.GGUF").is_ok());
    }
}
