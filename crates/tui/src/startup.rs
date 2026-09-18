use assistant_client::IpcClient;
use assistant_protocol::{HealthStatus, ModelState, ResponsePayload};
use std::{
    env,
    error::Error,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_MODEL_ROOT: &str = "AI/models";
const DEFAULT_EMBEDDING_MODEL: &str = "bge-small-en-v1.5-q8_0.gguf";
const DEFAULT_RUNTIME_LOG: &str = ".cache/assistant/runtime.log";
const RUNTIME_READY_TIMEOUT: Duration = Duration::from_secs(120);
const RUNTIME_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub filename: String,
    pub path: PathBuf,
}

impl ModelChoice {
    fn new(path: PathBuf) -> Option<Self> {
        let filename = path.file_name()?.to_str()?.to_string();
        Some(Self { filename, path })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub health: HealthStatus,
    pub model_status: ModelState,
    pub active_model_id: String,
    pub model_display_name: String,
}

#[derive(Debug)]
pub struct RuntimeLaunch {
    child: Child,
    detach_on_drop: bool,
}

impl RuntimeLaunch {
    pub fn new(
        runtime_bin: &Path,
        socket_path: &Path,
        model: &ModelChoice,
    ) -> Result<Self, Box<dyn Error>> {
        let log_path = runtime_log_path()?;
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| {
                format!("Failed to open runtime log {}: {error}", log_path.display())
            })?;
        let stderr = log_file
            .try_clone()
            .map_err(|error| format!("Failed to duplicate runtime log handle: {error}"))?;

        let mut command = Command::new(runtime_bin);
        command
            .arg("--daemon")
            .env("ASSISTANT_QWEN_MODEL", &model.filename)
            .env("ASSISTANT_SOCKET_PATH", socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(stderr));

        let child = command.spawn().map_err(|error| {
            format!("Failed to start runtime {}: {error}", runtime_bin.display())
        })?;

        Ok(Self {
            child,
            detach_on_drop: false,
        })
    }

    pub fn wait_until_ready(&mut self, socket_path: &Path) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + RUNTIME_READY_TIMEOUT;
        let client = IpcClient::new(socket_path).with_timeout(Duration::from_secs(2));

        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Err(runtime_exit_error(status));
            }

            if let Some(runtime) = probe_runtime_with_client(&client) {
                if runtime.health.ready && runtime.model_status.ready {
                    self.detach_on_drop = true;
                    return Ok(());
                }
            }

            thread::sleep(RUNTIME_POLL_INTERVAL);
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
        Err("Timed out waiting for assistant runtime to become ready.".into())
    }
}

impl Drop for RuntimeLaunch {
    fn drop(&mut self) {
        if self.detach_on_drop {
            return;
        }

        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

pub fn discover_models() -> Result<Vec<ModelChoice>, Box<dyn Error>> {
    let root = model_root()?;
    let embedding_model = env::var("ASSISTANT_EMBEDDING_MODEL")
        .unwrap_or_else(|_| DEFAULT_EMBEDDING_MODEL.to_string());
    discover_models_from(&root, &embedding_model)
}

pub fn discover_models_from(
    root: &Path,
    embedding_model: &str,
) -> Result<Vec<ModelChoice>, Box<dyn Error>> {
    if !root.exists() {
        return Err(format!("Local model directory does not exist: {}", root.display()).into());
    }
    if !root.is_dir() {
        return Err(format!("Local model path is not a directory: {}", root.display()).into());
    }

    let mut models = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !is_gguf(&path) {
            continue;
        }

        let Some(model) = ModelChoice::new(path) else {
            continue;
        };
        if model.filename == embedding_model {
            continue;
        }
        models.push(model);
    }

    models.sort_by(|left, right| {
        left.filename
            .to_ascii_lowercase()
            .cmp(&right.filename.to_ascii_lowercase())
            .then_with(|| left.filename.cmp(&right.filename))
    });

    if models.is_empty() {
        return Err(format!(
            "No local GGUF generation models were found in {}.",
            root.display()
        )
        .into());
    }

    Ok(models)
}

pub fn initial_model_index(models: &[ModelChoice]) -> usize {
    let preferred = env::var("ASSISTANT_QWEN_MODEL").ok();
    initial_model_index_with_preference(models, preferred.as_deref())
}

fn initial_model_index_with_preference(models: &[ModelChoice], preferred: Option<&str>) -> usize {
    preferred
        .and_then(|name| models.iter().position(|model| model.filename == name))
        .unwrap_or(0)
}

pub fn probe_runtime(socket_path: &Path) -> Option<RuntimeStatus> {
    let client = IpcClient::new(socket_path).with_timeout(Duration::from_millis(500));
    probe_runtime_with_client(&client)
}

fn probe_runtime_with_client(client: &IpcClient) -> Option<RuntimeStatus> {
    let health = match client.request(assistant_protocol::RequestMethod::Health) {
        Ok(ResponsePayload::Health(value)) => value,
        _ => return None,
    };
    let model_status = match client.request(assistant_protocol::RequestMethod::ModelStatus) {
        Ok(ResponsePayload::ModelStatus(value)) => value,
        _ => return None,
    };
    let active_model_id = model_status.active_model.clone()?;
    let model_display_name = match client.request(assistant_protocol::RequestMethod::ModelList) {
        Ok(ResponsePayload::Models(value)) => value
            .models
            .into_iter()
            .find(|model| model.id == active_model_id)
            .map(|model| model.display_name)
            .unwrap_or_else(|| active_model_id.clone()),
        _ => active_model_id.clone(),
    };

    Some(RuntimeStatus {
        health,
        model_status,
        active_model_id,
        model_display_name,
    })
}

pub fn runtime_binary_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = env::var_os("ASSISTANT_RUNTIME_BIN") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "ASSISTANT_RUNTIME_BIN does not point to a file: {}",
            path.display()
        )
        .into());
    }

    let current = env::current_exe()?;
    let parent = current
        .parent()
        .ok_or("Could not determine assistant-tui executable directory.")?;
    let sibling = parent.join("assistant");
    if sibling.is_file() {
        return Ok(sibling);
    }

    Err(format!(
        "Could not find runtime binary at {}. Set ASSISTANT_RUNTIME_BIN to override it.",
        sibling.display()
    )
    .into())
}

fn model_root() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = env::var_os("ASSISTANT_MODEL_ROOT") {
        return Ok(PathBuf::from(value));
    }

    let home = env::var_os("HOME").ok_or("HOME environment variable is not set.")?;
    Ok(PathBuf::from(home).join(DEFAULT_MODEL_ROOT))
}

fn runtime_log_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = env::var_os("ASSISTANT_RUNTIME_LOG") {
        let path = PathBuf::from(value);
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        return Ok(path);
    }

    let home = env::var_os("HOME").ok_or("HOME environment variable is not set.")?;
    let path = PathBuf::from(home).join(DEFAULT_RUNTIME_LOG);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    Ok(path)
}

fn runtime_exit_error(status: ExitStatus) -> Box<dyn Error> {
    format!("Assistant runtime exited before becoming ready: {status}").into()
}

fn is_gguf(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "assistant-tui-test-{}-{}",
                std::process::id(),
                suffix
            ));
            fs::create_dir_all(&path).expect("create temporary directory");
            Self { path }
        }

        fn write_file(&self, name: &str) {
            File::create(self.path.join(name)).expect("create test file");
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn discover_models_filters_non_gguf_and_embedding_model() {
        let directory = TestDir::new();
        directory.write_file("Qwen.gguf");
        directory.write_file("Mistral.GGUF");
        directory.write_file("bge-small-en-v1.5-q8_0.gguf");
        directory.write_file("notes.txt");
        fs::create_dir(directory.path.join("nested.gguf")).expect("create test directory");

        let models = discover_models_from(&directory.path, DEFAULT_EMBEDDING_MODEL)
            .expect("discover models");
        let names = models
            .iter()
            .map(|model| model.filename.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["Mistral.GGUF", "Qwen.gguf"]);
    }

    #[test]
    fn initial_model_index_defaults_to_first_model_without_preference() {
        let models = vec![
            ModelChoice {
                filename: "A.gguf".to_string(),
                path: PathBuf::from("/tmp/A.gguf"),
            },
            ModelChoice {
                filename: "B.gguf".to_string(),
                path: PathBuf::from("/tmp/B.gguf"),
            },
        ];

        assert_eq!(initial_model_index_with_preference(&models, None), 0);
    }

    #[test]
    fn initial_model_index_uses_matching_preference() {
        let models = vec![
            ModelChoice {
                filename: "A.gguf".to_string(),
                path: PathBuf::from("/tmp/A.gguf"),
            },
            ModelChoice {
                filename: "B.gguf".to_string(),
                path: PathBuf::from("/tmp/B.gguf"),
            },
        ];

        assert_eq!(
            initial_model_index_with_preference(&models, Some("B.gguf")),
            1
        );
        assert_eq!(
            initial_model_index_with_preference(&models, Some("missing.gguf")),
            0
        );
    }

    #[test]
    fn discover_models_rejects_empty_generation_directory() {
        let directory = TestDir::new();
        directory.write_file("bge-small-en-v1.5-q8_0.gguf");

        let error = discover_models_from(&directory.path, DEFAULT_EMBEDDING_MODEL)
            .expect_err("empty generation model directory should fail");
        assert!(
            error
                .to_string()
                .contains("No local GGUF generation models")
        );
    }
}
