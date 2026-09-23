use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermission {
    ReadOnly,
    ApprovalRequired,
    Destructive,
}

impl ToolPermission {
    pub fn requires_approval(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub permission: ToolPermission,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: Value,
}

pub trait Tool: Send {
    fn definition(&self) -> ToolDefinition;

    fn execute(&self, arguments: &Value) -> Result<ToolResult>;
}

pub struct SystemInfoTool {
    generation_url: String,
    embedding_url: String,
    socket_path: Option<PathBuf>,
}

impl SystemInfoTool {
    pub fn new(
        generation_url: impl Into<String>,
        embedding_url: impl Into<String>,
        socket_path: Option<PathBuf>,
    ) -> Self {
        Self {
            generation_url: generation_url.into(),
            embedding_url: embedding_url.into(),
            socket_path,
        }
    }

    fn scope(arguments: &Value) -> Result<&str> {
        let object = arguments
            .as_object()
            .ok_or("system.info arguments must be an object.")?;

        for key in object.keys() {
            if key != "scope" {
                return Err(format!("system.info does not accept argument '{key}'.").into());
            }
        }

        let scope = object
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("summary")
            .trim();

        if !matches!(
            scope,
            "summary" | "hardware" | "runtime" | "network" | "all"
        ) {
            return Err(
                "system.info scope must be summary, hardware, runtime, network, or all.".into(),
            );
        }

        Ok(scope)
    }

    fn read_proc_value(name: &str) -> Option<String> {
        fs::read_to_string(name)
            .ok()
            .map(|value| value.trim().to_string())
    }

    fn cpu_model() -> Option<String> {
        let contents = fs::read_to_string("/proc/cpuinfo").ok()?;
        contents.lines().find_map(|line| {
            line.strip_prefix("model name")
                .and_then(|value| value.split_once(':'))
                .map(|(_, value)| value.trim().to_string())
        })
    }

    fn memory_info() -> Value {
        let contents = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mut values = std::collections::HashMap::<&str, u64>::new();

        for line in contents.lines() {
            let Some((key, remainder)) = line.split_once(':') else {
                continue;
            };
            let Some(value) = remainder.split_whitespace().next() else {
                continue;
            };
            if let Ok(value) = value.parse::<u64>() {
                values.insert(key, value * 1024);
            }
        }

        let total = values.get("MemTotal").copied();
        let available = values.get("MemAvailable").copied();
        let used = total
            .zip(available)
            .map(|(total, available)| total.saturating_sub(available));
        let swap_total = values.get("SwapTotal").copied();
        let swap_free = values.get("SwapFree").copied();
        let swap_used = swap_total
            .zip(swap_free)
            .map(|(total, free)| total.saturating_sub(free));

        serde_json::json!({
            "total_bytes": total,
            "available_bytes": available,
            "used_bytes": used,
            "swap_total_bytes": swap_total,
            "swap_used_bytes": swap_used,
        })
    }

    fn gpu_info() -> Value {
        if let Ok(output) = Command::new("nvidia-smi")
            .args([
                "--query-gpu=name,memory.total,memory.used,utilization.gpu,driver_version",
                "--format=csv,noheader,nounits",
            ])
            .output()
        {
            if output.status.success() {
                let gpus = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
                        if parts.len() != 5 {
                            return None;
                        }
                        Some(serde_json::json!({
                            "name": parts[0],
                            "vram_total_mib": parts[1].parse::<u64>().ok(),
                            "vram_used_mib": parts[2].parse::<u64>().ok(),
                            "utilization_percent": parts[3].parse::<u64>().ok(),
                            "driver": parts[4],
                        }))
                    })
                    .collect::<Vec<_>>();

                if !gpus.is_empty() {
                    return serde_json::json!({
                        "available": true,
                        "source": "nvidia-smi",
                        "devices": gpus,
                    });
                }
            }
        }

        if let Ok(output) = Command::new("lspci").output() {
            if output.status.success() {
                let devices = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| {
                        let lower = line.to_ascii_lowercase();
                        lower.contains("vga compatible controller")
                            || lower.contains("3d controller")
                            || lower.contains("display controller")
                    })
                    .map(|line| serde_json::json!({"description": line.trim()}))
                    .collect::<Vec<_>>();

                if !devices.is_empty() {
                    return serde_json::json!({
                        "available": true,
                        "source": "lspci",
                        "devices": devices,
                    });
                }
            }
        }

        serde_json::json!({
            "available": false,
            "devices": [],
        })
    }

    #[cfg(unix)]
    fn storage_info() -> Value {
        let path = std::ffi::CString::new("/").expect("literal path has no NUL");
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();

        let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
        if result != 0 {
            return serde_json::json!({"available": false});
        }

        let stats = unsafe { stats.assume_init() };
        let block_size = stats.f_frsize as u64;
        let total = (stats.f_blocks as u64).saturating_mul(block_size);
        let available = (stats.f_bavail as u64).saturating_mul(block_size);
        let used = total.saturating_sub(available);

        serde_json::json!({
            "available": true,
            "mount": "/",
            "total_bytes": total,
            "available_bytes": available,
            "used_bytes": used,
        })
    }

    #[cfg(not(unix))]
    fn storage_info() -> Value {
        serde_json::json!({"available": false})
    }

    fn battery_info() -> Value {
        let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
            return serde_json::json!({"available": false});
        };

        let mut batteries = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("BAT") {
                continue;
            }

            let base = entry.path();
            let capacity = fs::read_to_string(base.join("capacity"))
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok());
            let status = fs::read_to_string(base.join("status"))
                .ok()
                .map(|value| value.trim().to_string());

            batteries.push(serde_json::json!({
                "name": name,
                "capacity_percent": capacity,
                "status": status,
            }));
        }

        serde_json::json!({
            "available": !batteries.is_empty(),
            "batteries": batteries,
        })
    }

    fn network_info() -> Value {
        let mut interfaces = Vec::new();
        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name == "lo" {
                    continue;
                }
                let state = fs::read_to_string(entry.path().join("operstate"))
                    .ok()
                    .map(|value| value.trim().to_string());
                interfaces.push(serde_json::json!({
                    "name": name,
                    "state": state,
                }));
            }
        }

        let default_interface = fs::read_to_string("/proc/net/route")
            .ok()
            .and_then(|contents| {
                contents.lines().skip(1).find_map(|line| {
                    let fields = line.split_whitespace().collect::<Vec<_>>();
                    (fields.len() > 1 && fields[1] == "00000000").then(|| fields[0].to_string())
                })
            });

        serde_json::json!({
            "interfaces": interfaces,
            "default_interface": default_interface,
        })
    }

    fn service_models(endpoint: &str) -> Value {
        let endpoint = format!("{}/v1/models", endpoint.trim_end_matches('/'));
        let client = match reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(500))
            .timeout(std::time::Duration::from_secs(1))
            .build()
        {
            Ok(client) => client,
            Err(_) => return serde_json::json!({"reachable": false, "models": []}),
        };

        let response = match client.get(endpoint).send() {
            Ok(response) => response,
            Err(_) => return serde_json::json!({"reachable": false, "models": []}),
        };

        if !response.status().is_success() {
            return serde_json::json!({"reachable": false, "models": []});
        }

        let body = match response.json::<Value>() {
            Ok(body) => body,
            Err(_) => return serde_json::json!({"reachable": true, "models": []}),
        };

        let models = body
            .get("data")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        serde_json::json!({
            "reachable": true,
            "models": models,
        })
    }

    fn clock_info() -> Value {
        let now_utc = OffsetDateTime::now_utc();
        let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let now_local = now_utc.to_offset(local_offset);
        let timezone_offset = {
            let seconds = local_offset.whole_seconds();
            let sign = if seconds < 0 { '-' } else { '+' };
            let absolute = seconds.unsigned_abs();
            format!("{sign}{:02}:{:02}", absolute / 3600, (absolute % 3600) / 60)
        };

        serde_json::json!({
            "utc": now_utc
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string()),
            "local": now_local
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string()),
            "date": now_local.date().to_string(),
            "time": format!(
                "{:02}:{:02}:{:02}",
                now_local.hour(),
                now_local.minute(),
                now_local.second(),
            ),
            "timezone_offset": timezone_offset
        })
    }

    fn runtime_info(&self) -> Value {
        let generation = Self::service_models(&self.generation_url);
        let embedding = Self::service_models(&self.embedding_url);
        let socket_exists = self.socket_path.as_ref().is_some_and(|path| path.exists());

        serde_json::json!({
            "pid": std::process::id(),
            "socket": self.socket_path.as_ref().map(|path| path.to_string_lossy().to_string()),
            "socket_exists": socket_exists,
            "system_uptime_seconds": Self::read_proc_value("/proc/uptime")
                .and_then(|value| value.split_whitespace().next().map(str::to_string))
                .and_then(|value| value.parse::<f64>().ok()),
            "checked_at_utc": OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string()),
            "clock": Self::clock_info(),
            "generation": {
                "endpoint": &self.generation_url,
                "reachable": generation["reachable"],
                "models": generation["models"],
                "active_model": generation["models"].as_array().and_then(|models| {
                    models.first().and_then(Value::as_str)
                }),
            },
            "embedding": {
                "endpoint": &self.embedding_url,
                "reachable": embedding["reachable"],
                "models": embedding["models"],
            },
        })
    }

    fn hardware_info() -> Value {
        serde_json::json!({
            "os": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "kernel": Self::read_proc_value("/proc/sys/kernel/osrelease"),
            "hostname": Self::read_proc_value("/etc/hostname"),
            "cpu": {
                "model": Self::cpu_model(),
                "logical_processors": std::thread::available_parallelism().map(|value| value.get()).ok(),
            },
            "memory": Self::memory_info(),
            "gpu": Self::gpu_info(),
            "storage": Self::storage_info(),
            "battery": Self::battery_info(),
        })
    }
}

impl Tool for SystemInfoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "system.info".to_string(),
            description: "Read live local machine and Zaraki runtime information without accessing persistent memory or secrets.".to_string(),
            permission: ToolPermission::ReadOnly,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let scope = Self::scope(arguments)?;
        let output = match scope {
            "summary" => serde_json::json!({
                "hardware": {
                    "os": std::env::consts::OS,
                    "architecture": std::env::consts::ARCH,
                    "cpu_model": Self::cpu_model(),
                    "logical_processors": std::thread::available_parallelism().map(|value| value.get()).ok(),
                    "memory": Self::memory_info(),
                },
                "runtime": self.runtime_info(),
            }),
            "hardware" => Self::hardware_info(),
            "runtime" => self.runtime_info(),
            "network" => Self::network_info(),
            "all" => serde_json::json!({
                "hardware": Self::hardware_info(),
                "runtime": self.runtime_info(),
                "network": Self::network_info(),
            }),
            _ => unreachable!(),
        };

        Ok(ToolResult {
            success: true,
            output,
        })
    }
}

pub struct FileReadTool {
    root: PathBuf,
}

impl FileReadTool {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn resolve(&self, requested: &str) -> Result<PathBuf> {
        let relative = Path::new(requested);

        if relative.is_absolute() {
            return Err("Absolute paths are not allowed.".into());
        }

        let path = self.root.join(relative);

        let canonical_root = fs::canonicalize(&self.root)?;

        let canonical_path = fs::canonicalize(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!("files.read path does not exist: {}", requested)
            } else {
                format!("files.read could not access '{}': {}", requested, error)
            }
        })?;

        if !canonical_path.starts_with(&canonical_root) {
            return Err("Path escapes tool root.".into());
        }

        if canonical_path.is_dir() {
            return Err(format!("files.read path is a directory: {}", requested).into());
        }

        Ok(canonical_path)
    }
}

impl Tool for FileReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "files.read".to_string(),
            description: "Read a text file inside the configured tool root.".to_string(),
            permission: ToolPermission::ReadOnly,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .ok_or("files.read requires a string 'path'.")?;

        if path.trim().is_empty() {
            return Err("files.read path cannot be empty.".into());
        }

        let resolved = self.resolve(path)?;

        let text = fs::read_to_string(&resolved)?;

        Ok(ToolResult {
            success: true,
            output: serde_json::json!({
                "path": path,
                "text": text
            }),
        })
    }
}

pub struct FileListTool {
    root: PathBuf,
}

impl FileListTool {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn resolve(&self, requested: &str) -> Result<PathBuf> {
        let relative = Path::new(requested);

        if relative.is_absolute() {
            return Err("Absolute paths are not allowed.".into());
        }

        let path = self.root.join(relative);

        let canonical_root = fs::canonicalize(&self.root)?;

        let canonical_path = fs::canonicalize(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!("files.list path does not exist: {}", requested)
            } else {
                format!("files.list could not access '{}': {}", requested, error)
            }
        })?;

        if !canonical_path.starts_with(&canonical_root) {
            return Err("Path escapes tool root.".into());
        }

        if !canonical_path.is_dir() {
            return Err(format!("files.list path is not a directory: {}", requested).into());
        }

        Ok(canonical_path)
    }

    fn list_entries(&self, requested: &str, recursive: bool, limit: usize) -> Result<ToolResult> {
        let canonical_root = fs::canonicalize(&self.root)?;

        let directory = self.resolve(requested)?;

        let mut entries = Vec::<serde_json::Value>::new();

        let mut pending = vec![directory];

        while let Some(current) = pending.pop() {
            let read_dir = fs::read_dir(&current)?;

            for entry in read_dir {
                if entries.len() >= limit {
                    return Ok(ToolResult {
                        success: true,
                        output: serde_json::json!({
                            "path": requested,
                            "entries": entries,
                            "count": entries.len(),
                            "truncated": true
                        }),
                    });
                }

                let entry = entry?;

                let metadata = fs::symlink_metadata(entry.path())?;

                if metadata.file_type().is_symlink() {
                    continue;
                }

                let canonical = fs::canonicalize(entry.path())?;

                if !canonical.starts_with(&canonical_root) {
                    continue;
                }

                let relative = canonical
                    .strip_prefix(&canonical_root)
                    .map_err(|_| "Could not construct relative file path.")?;

                let is_directory = canonical.is_dir();

                entries.push(serde_json::json!({
                    "path":
                        relative
                            .to_string_lossy()
                            .to_string(),
                    "type":
                        if is_directory {
                            "directory"
                        } else {
                            "file"
                        }
                }));

                if recursive && is_directory {
                    pending.push(canonical);
                }
            }
        }

        entries.sort_by(|a, b| {
            a["path"]
                .as_str()
                .unwrap_or("")
                .cmp(b["path"].as_str().unwrap_or(""))
        });

        Ok(ToolResult {
            success: true,
            output: serde_json::json!({
                "path": requested,
                "entries": entries,
                "count": entries.len(),
                "truncated": false
            }),
        })
    }
}

impl Tool for FileListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "files.list".to_string(),
            description: "List files and directories inside the configured tool root.".to_string(),
            permission: ToolPermission::ReadOnly,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .ok_or("files.list requires a string 'path'.")?;

        if path.trim().is_empty() {
            return Err("files.list path cannot be empty.".into());
        }

        let recursive = arguments
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(100) as usize;

        if limit == 0 || limit > 500 {
            return Err("files.list limit must be 1..=500.".into());
        }

        if arguments.as_object().map(|object| {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "path" | "recursive" | "limit"))
        }) != Some(true)
        {
            return Err("files.list accepts only path, recursive, and limit.".into());
        }

        self.list_entries(path, recursive, limit)
    }
}

pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: std::collections::HashMap::new(),
        }
    }

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        let definition = tool.definition();

        self.tools.insert(definition.name, Box::new(tool));
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn definition(&self, name: &str) -> Result<ToolDefinition> {
        self.tools
            .get(name)
            .map(|tool| tool.definition())
            .ok_or_else(|| format!("Unknown tool: {name}").into())
    }

    pub fn execute(&self, call: &ToolCall) -> Result<ToolResult> {
        let tool = self
            .tools
            .get(&call.tool)
            .ok_or_else(|| format!("Unknown tool: {}", call.tool))?;

        tool.execute(&call.arguments)
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn system_info_is_read_only() -> Result<()> {
        let tool = SystemInfoTool::new("http://127.0.0.1:9", "http://127.0.0.1:9", None);

        assert_eq!(tool.definition().permission, ToolPermission::ReadOnly);

        let result = tool.execute(&serde_json::json!({"scope": "hardware"}))?;
        assert!(result.success);
        assert!(result.output.get("cpu").is_some());
        assert!(result.output.get("memory").is_some());
        assert!(result.output.get("gpu").is_some());

        Ok(())
    }

    #[test]
    fn system_info_rejects_unknown_scope() {
        let tool = SystemInfoTool::new("http://127.0.0.1:9", "http://127.0.0.1:9", None);

        assert!(
            tool.execute(&serde_json::json!({"scope": "secrets"}))
                .is_err()
        );
    }

    #[test]
    fn file_read_is_read_only() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let file = dir.path().join("hello.txt");

        let mut handle = fs::File::create(&file)?;

        writeln!(handle, "hello")?;

        let tool = FileReadTool::new(dir.path());

        let result = tool.execute(&serde_json::json!({
            "path": "hello.txt"
        }))?;

        assert!(result.success);

        assert!(result.output["text"].as_str().unwrap().contains("hello"));

        Ok(())
    }

    #[test]
    fn file_read_rejects_directory() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = FileReadTool::new(dir.path());

        let error = tool
            .execute(&serde_json::json!({
                "path": "."
            }))
            .expect_err("files.read should reject directories")
            .to_string();

        assert!(error.contains("files.read path is a directory"));

        Ok(())
    }

    #[test]
    fn file_read_reports_missing_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = FileReadTool::new(dir.path());

        let error = tool
            .execute(&serde_json::json!({
                "path": "missing.txt"
            }))
            .expect_err("files.read should reject missing files")
            .to_string();

        assert!(error.contains("files.read path does not exist"));

        Ok(())
    }

    #[test]
    fn file_read_rejects_absolute_paths() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let tool = FileReadTool::new(dir.path());

        assert!(
            tool.execute(&serde_json::json!({
                "path": "/etc/passwd"
            }))
            .is_err()
        );

        Ok(())
    }

    #[test]
    fn file_list_lists_directory() -> Result<()> {
        let dir = tempfile::tempdir()?;

        fs::create_dir(dir.path().join("nested"))?;

        fs::write(dir.path().join("hello.txt"), "hello")?;

        fs::write(dir.path().join("nested/inside.txt"), "inside")?;

        let tool = FileListTool::new(dir.path());

        let result = tool.execute(&serde_json::json!({
            "path": ".",
            "recursive": false,
            "limit": 100
        }))?;

        assert!(result.success);

        let entries = result.output["entries"]
            .as_array()
            .ok_or("entries was not an array.")?;

        assert!(
            entries
                .iter()
                .any(|entry| { entry["path"] == "hello.txt" && entry["type"] == "file" })
        );

        assert!(
            entries
                .iter()
                .any(|entry| { entry["path"] == "nested" && entry["type"] == "directory" })
        );

        Ok(())
    }

    #[test]
    fn file_list_recursive_finds_nested_files() -> Result<()> {
        let dir = tempfile::tempdir()?;

        fs::create_dir_all(dir.path().join("a/b"))?;

        fs::write(dir.path().join("a/b/test.txt"), "test")?;

        let tool = FileListTool::new(dir.path());

        let result = tool.execute(&serde_json::json!({
            "path": ".",
            "recursive": true,
            "limit": 100
        }))?;

        let entries = result.output["entries"]
            .as_array()
            .ok_or("entries was not an array.")?;

        assert!(
            entries
                .iter()
                .any(|entry| { entry["path"] == "a/b/test.txt" })
        );

        Ok(())
    }

    #[test]
    fn file_list_rejects_absolute_paths() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let tool = FileListTool::new(dir.path());

        assert!(
            tool.execute(&serde_json::json!({
                "path": "/etc",
                "recursive": false
            }))
            .is_err()
        );

        Ok(())
    }

    #[test]
    fn file_list_rejects_missing_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = FileListTool::new(dir.path());

        let error = tool
            .execute(&serde_json::json!({
                "path": "missing"
            }))
            .expect_err("files.list should reject missing paths")
            .to_string();

        assert!(error.contains("files.list path does not exist"));

        Ok(())
    }

    #[test]
    fn file_list_rejects_file() -> Result<()> {
        let dir = tempfile::tempdir()?;

        fs::write(dir.path().join("hello.txt"), "hello")?;

        let tool = FileListTool::new(dir.path());

        let error = tool
            .execute(&serde_json::json!({
                "path": "hello.txt"
            }))
            .expect_err("files.list should reject files")
            .to_string();

        assert!(error.contains("files.list path is not a directory"));

        Ok(())
    }

    #[test]
    fn tool_permissions_encode_approval_policy() -> Result<()> {
        assert!(!ToolPermission::ReadOnly.requires_approval());
        assert!(ToolPermission::ApprovalRequired.requires_approval());
        assert!(ToolPermission::Destructive.requires_approval());

        let json = serde_json::to_string(&ToolPermission::ApprovalRequired)?;
        assert_eq!(json, "\"approval_required\"");

        Ok(())
    }

    #[test]
    fn memory_write_rejects_mismatched_task_and_reminder_metadata() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());

        let result = tool.execute(&serde_json::json!({
            "path": "journal/bad.md",
            "title": "Bad",
            "type": "journal",
            "body": "Bad",
            "memory_kind": "task",
            "task_status": "open",
            "reminder_status": "scheduled"
        }));

        assert!(result.is_err());
        assert!(!dir.path().join("journal/bad.md").exists());
        Ok(())
    }

    #[test]
    fn registry_exposes_canonical_tool_permissions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut registry = ToolRegistry::new();

        registry.register(FileReadTool::new(dir.path()));
        registry.register(FileListTool::new(dir.path()));
        registry.register(MemoryWriteTool::new(dir.path()));

        assert_eq!(
            registry.definition("files.read")?.permission,
            ToolPermission::ReadOnly
        );
        assert_eq!(
            registry.definition("files.list")?.permission,
            ToolPermission::ReadOnly
        );
        assert_eq!(
            registry.definition("memory.write")?.permission,
            ToolPermission::ApprovalRequired
        );

        registry.register(TaskListTool::new(dir.path().join("assistant.db"))?);
        registry.register(TaskMutationTool::new(
            dir.path(),
            dir.path().join("assistant.db"),
        )?);
        registry.register(ReminderListTool::new(dir.path().join("assistant.db"))?);
        registry.register(ReminderMutationTool::new(
            dir.path(),
            dir.path().join("assistant.db"),
        )?);

        assert_eq!(
            registry.definition("tasks.list")?.permission,
            ToolPermission::ReadOnly
        );
        assert_eq!(
            registry.definition("tasks.mutate")?.permission,
            ToolPermission::ApprovalRequired
        );
        assert_eq!(
            registry.definition("reminders.list")?.permission,
            ToolPermission::ReadOnly
        );
        assert_eq!(
            registry.definition("reminders.mutate")?.permission,
            ToolPermission::ApprovalRequired
        );

        Ok(())
    }

    #[test]
    fn registry_rejects_unknown_tools() -> Result<()> {
        let registry = ToolRegistry::new();

        let result = registry.execute(&ToolCall {
            tool: "calendar.create".to_string(),
            arguments: serde_json::json!({}),
        });

        assert!(result.is_err());

        Ok(())
    }
}

pub mod calendar;
mod calendar_sync;
pub mod memory;
pub mod tasks;

pub use calendar::{
    GoogleCalendarAuth, GoogleCalendarClient, GoogleCalendarCreateEventTool,
    GoogleCalendarGetEventTool, GoogleCalendarListEventsTool, GoogleCalendarUpdateEventTool,
};
pub use memory::{
    MemoryWriteEntity, MemoryWriteEvent, MemoryWriteRelationship, MemoryWriteRequest,
    MemoryWriteState, MemoryWriteTool,
};
pub use tasks::{ReminderListTool, ReminderMutationTool, TaskListTool, TaskMutationTool};

#[cfg(test)]
mod memory_write_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_write_creates_markdown() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let tool = MemoryWriteTool::new(dir.path());

        let result = tool.execute(&json!({
            "path":
                "journal/test.md",
            "title":
                "Test Memory",
            "type":
                "journal",
            "body":
                "This is a test memory.",
            "tags": [
                "test",
                "assistant"
            ]
        }))?;

        assert!(result.success);

        let contents = std::fs::read_to_string(dir.path().join("journal/test.md"))?;

        assert!(contents.contains("title: \"Test Memory\""));

        assert!(contents.contains("type: \"journal\""));

        assert!(contents.contains("This is a test memory."));

        Ok(())
    }

    #[test]
    fn memory_write_renders_structured_metadata() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());

        tool.execute(&json!({
            "path": "journal/structured.md",
            "title": "Structured Memory",
            "type": "journal",
            "body": "A structured memory.",
            "entities": [{"name": "Qwen3.5", "type": "technology"}],
            "relationships": [{"source": "Qwen3.5", "target": "Personal Assistant", "relationship": "used_by"}],
            "events": [{"type": "decision", "occurred_at": "2026-09-12", "description": "Selected the local model.", "entities": ["Qwen3.5"]}],
            "states": [{"entity": "Qwen3.5", "state": "selected", "valid_from": "2026-09-12"}]
        }))?;

        let contents = std::fs::read_to_string(dir.path().join("journal/structured.md"))?;
        assert!(contents.contains("entities:\n"));
        assert!(contents.contains("  - name: \"Qwen3.5\"\n    type: \"technology\"\n"));
        assert!(contents.contains("relationships:\n"));
        assert!(contents.contains("  - source: \"Qwen3.5\"\n    target: \"Personal Assistant\"\n    relationship: \"used_by\"\n"));
        assert!(contents.contains("events:\n"));
        assert!(contents.contains("  - type: \"decision\"\n    occurred_at: \"2026-09-12\"\n    description: \"Selected the local model.\"\n    entities:\n      - \"Qwen3.5\"\n"));
        assert!(contents.contains("states:\n"));
        assert!(contents.contains(
            "  - entity: \"Qwen3.5\"\n    state: \"selected\"\n    valid_from: \"2026-09-12\"\n"
        ));

        Ok(())
    }

    #[test]
    fn memory_write_rejects_structured_metadata_overflow() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());
        let entities = (0..9)
            .map(|index| json!({"name": format!("Entity {index}"), "type": "test"}))
            .collect::<Vec<_>>();

        assert!(
            tool.execute(&json!({
                "path": "journal/overflow.md",
                "title": "Overflow",
                "type": "journal",
                "body": "Should fail.",
                "entities": entities
            }))
            .is_err()
        );
        assert!(!dir.path().join("journal/overflow.md").exists());

        Ok(())
    }

    #[test]
    fn memory_write_supersedes_state_in_existing_note() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());
        tool.write_request_idempotent(&MemoryWriteRequest {
            path: "journal/state.md".to_string(),
            title: "Project state".to_string(),
            note_type: "journal".to_string(),
            body: "Project is active.".to_string(),
            tags: vec![],
            entities: vec![MemoryWriteEntity {
                name: "Project".to_string(),
                entity_type: "project".to_string(),
            }],
            relationships: vec![],
            events: vec![],
            states: vec![MemoryWriteState {
                entity: "Project".to_string(),
                state: "active".to_string(),
                valid_from: Some("2026-01-01".to_string()),
                valid_to: None,
            }],
            memory_kind: Some("state".to_string()),
            task_status: None,
            reminder_status: None,
            due_at: None,
            source_type: None,
            source_id: None,
            source_path: None,
            source_anchor: None,
            source_excerpt: None,
        })?;
        tool.supersede_state_in_file("journal/state.md", "Project", "active", "2026-09-11")?;
        let contents = std::fs::read_to_string(dir.path().join("journal/state.md"))?;
        assert!(contents.contains("valid_to: \"2026-09-11\""));
        assert!(
            contents.find("valid_to: \"2026-09-11\"") < contents.find("memory_kind: \"state\"")
        );
        tool.supersede_state_in_file("journal/state.md", "Project", "active", "2026-09-11")?;
        Ok(())
    }

    #[test]
    fn memory_write_renders_provenance() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());
        tool.execute(&json!({
            "path": "journal/provenance.md",
            "title": "Provenance",
            "type": "journal",
            "body": "Body",
            "source_type": "conversation",
            "source_id": "conversation_turn:1",
            "source_path": "session:abc",
            "source_anchor": "turn:1",
            "source_excerpt": "Remember this."
        }))?;
        let contents = std::fs::read_to_string(dir.path().join("journal/provenance.md"))?;
        assert!(contents.contains("source_type: \"conversation\""));
        assert!(contents.contains("source_id: \"conversation_turn:1\""));
        assert!(contents.contains("source_anchor: \"turn:1\""));
        Ok(())
    }

    #[test]
    fn memory_write_renders_task_and_reminder_metadata() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());

        tool.execute(&json!({
            "path": "journal/task.md",
            "title": "Task",
            "type": "journal",
            "body": "Do the thing.",
            "memory_kind": "task",
            "task_status": "open",
            "due_at": "2026-09-20T10:00:00Z"
        }))?;

        let task = std::fs::read_to_string(dir.path().join("journal/task.md"))?;
        assert!(task.contains("memory_kind: \"task\""));
        assert!(task.contains("task_status: \"open\""));
        assert!(task.contains("due_at: \"2026-09-20T10:00:00Z\""));

        tool.execute(&json!({
            "path": "journal/reminder.md",
            "title": "Reminder",
            "type": "journal",
            "body": "Ping me.",
            "memory_kind": "reminder",
            "reminder_status": "scheduled",
            "due_at": "2026-09-21T09:00:00Z"
        }))?;

        let reminder = std::fs::read_to_string(dir.path().join("journal/reminder.md"))?;
        assert!(reminder.contains("memory_kind: \"reminder\""));
        assert!(reminder.contains("reminder_status: \"scheduled\""));
        Ok(())
    }

    #[test]
    fn memory_write_renders_memory_kind() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());
        tool.execute(&json!({
            "path": "journal/kind.md",
            "title": "Kind",
            "type": "journal",
            "body": "Body",
            "memory_kind": "preference"
        }))?;
        let contents = std::fs::read_to_string(dir.path().join("journal/kind.md"))?;
        assert!(contents.contains("memory_kind: \"preference\""));
        Ok(())
    }

    #[test]
    fn memory_write_idempotent_accepts_identical_existing_content() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());
        let request = MemoryWriteRequest {
            path: "journal/idempotent.md".to_string(),
            title: "Idempotent".to_string(),
            note_type: "journal".to_string(),
            body: "Body".to_string(),
            tags: vec!["test".to_string()],
            entities: vec![],
            relationships: vec![],
            events: vec![],
            states: vec![],
            memory_kind: None,
            task_status: None,
            reminder_status: None,
            due_at: None,
            source_type: None,
            source_id: None,
            source_path: None,
            source_anchor: None,
            source_excerpt: None,
        };
        let first = tool.write_request_idempotent(&request)?;
        let second = tool.write_request_idempotent(&request)?;
        assert_eq!(first.output["created"], true);
        assert_eq!(second.output["created"], false);
        assert_eq!(second.output["already_present"], true);
        Ok(())
    }

    #[test]
    fn memory_write_idempotent_rejects_different_existing_content() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = MemoryWriteTool::new(dir.path());
        let first = MemoryWriteRequest {
            path: "journal/idempotent.md".to_string(),
            title: "Idempotent".to_string(),
            note_type: "journal".to_string(),
            body: "Body A".to_string(),
            tags: vec![],
            entities: vec![],
            relationships: vec![],
            events: vec![],
            states: vec![],
            memory_kind: None,
            task_status: None,
            reminder_status: None,
            due_at: None,
            source_type: None,
            source_id: None,
            source_path: None,
            source_anchor: None,
            source_excerpt: None,
        };
        let mut second = first.clone();
        second.body = "Body B".to_string();
        tool.write_request_idempotent(&first)?;
        assert!(tool.write_request_idempotent(&second).is_err());
        Ok(())
    }

    #[test]
    fn memory_write_never_overwrites() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let tool = MemoryWriteTool::new(dir.path());

        let arguments = json!({
            "path": "journal/test.md",
            "title": "First",
            "type": "journal",
            "body": "Original"
        });

        tool.execute(&arguments)?;

        assert!(
            tool.execute(&json!({
                "path": "journal/test.md",
                "title": "Second",
                "type": "journal",
                "body": "Replacement"
            }))
            .is_err()
        );

        let contents = std::fs::read_to_string(dir.path().join("journal/test.md"))?;

        assert!(contents.contains("Original"));

        assert!(!contents.contains("Replacement"));

        Ok(())
    }

    #[test]
    fn memory_write_rejects_escape() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let tool = MemoryWriteTool::new(dir.path());

        assert!(
            tool.execute(&json!({
                "path":
                    "../escape.md",
                "title":
                    "Bad",
                "type":
                    "journal",
                "body":
                    "Should fail"
            }))
            .is_err()
        );

        Ok(())
    }
}

#[test]
fn memory_write_policy_rejects_non_journal_storage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let tool = MemoryWriteTool::new(dir.path());

    assert!(
        tool.execute(&serde_json::json!({
            "path": "projects/test.md",
            "title": "Bad Path",
            "type": "journal",
            "body": "Should fail"
        }))
        .is_err()
    );

    assert!(
        tool.execute(&serde_json::json!({
            "path": "journal/test.md",
            "title": "Bad Type",
            "type": "fact",
            "body": "Should fail"
        }))
        .is_err()
    );

    assert!(
        tool.execute(&serde_json::json!({
            "path": "journal/test.md",
            "title": "Bad Tags",
            "type": "journal",
            "body": "Should fail",
            "tags": ["1", "2", "3", "4", "5", "6", "7", "8", "9"]
        }))
        .is_err()
    );

    Ok(())
}

#[cfg(test)]
mod approved_execution_tests {
    use super::*;

    #[test]
    fn approved_memory_write_is_executed() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let mut registry = ToolRegistry::new();

        registry.register(MemoryWriteTool::new(dir.path()));

        let result = registry.execute(&ToolCall {
            tool: "memory.write".to_string(),
            arguments: serde_json::json!({
                "path": "journal/approved.md",
                "title": "Approved",
                "type": "journal",
                "body": "Approved write."
            }),
        })?;

        assert!(result.success);

        let contents = std::fs::read_to_string(dir.path().join("journal/approved.md"))?;

        assert!(contents.contains("Approved write."));

        Ok(())
    }
}
