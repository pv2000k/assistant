mod ipc;

use assistant_protocol::{
    HealthStatus, ModelInfo, ModelList, ModelState, RequestMethod, ResponsePayload, WireRequest,
    WireResponse,
};
use model_router::ModelRouter;
use orchestrator::{
    ApprovalHandler, Controller, HybridMemory, IndexedToolExecutor, LlamaCppController,
    Orchestrator, PersistentMemoryIndexer, ToolCall,
};
use sqlite_memory::SqliteMemoryDb;
use std::{
    collections::HashMap,
    env,
    error::Error,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

const MAX_SESSION_TURNS: usize = 8;
const MODEL_PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const MODEL_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_HISTORY_CHARS: usize = 5000;
const MAX_HISTORY_TURN_CHARS: usize = 1200;

#[derive(Debug, Clone)]
struct LocalModelDefinition {
    id: String,
    display_name: String,
    base_url: String,
    model: String,
    capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
struct ModelSession {
    models: Arc<HashMap<String, LocalModelDefinition>>,
    active_model: Arc<RwLock<String>>,
}

impl ModelSession {
    fn from_environment(qwen_url: &str, qwen_model: &str) -> Result<Self, Box<dyn Error>> {
        let mut models = HashMap::new();
        models.insert(
            "qwen".to_string(),
            LocalModelDefinition {
                id: "qwen".to_string(),
                display_name: "Qwen3.5 4B".to_string(),
                base_url: qwen_url.to_string(),
                model: qwen_model.to_string(),
                capabilities: vec!["controller".to_string(), "general_response".to_string()],
            },
        );

        if let Ok(raw) = env::var("ASSISTANT_LOCAL_MODELS") {
            let configured = serde_json::from_str::<Vec<serde_json::Value>>(&raw)
                .map_err(|error| format!("ASSISTANT_LOCAL_MODELS is invalid JSON: {error}"))?;

            for item in configured {
                let id = item
                    .get("id")
                    .and_then(|value| value.as_str())
                    .ok_or("ASSISTANT_LOCAL_MODELS entries require a string id.")?
                    .trim()
                    .to_ascii_lowercase();
                let display_name = item
                    .get("display_name")
                    .and_then(|value| value.as_str())
                    .unwrap_or(&id)
                    .trim()
                    .to_string();
                let base_url = item
                    .get("url")
                    .and_then(|value| value.as_str())
                    .ok_or("ASSISTANT_LOCAL_MODELS entries require a string url.")?
                    .trim()
                    .to_string();
                let model = item
                    .get("model")
                    .and_then(|value| value.as_str())
                    .ok_or("ASSISTANT_LOCAL_MODELS entries require a string model.")?
                    .trim()
                    .to_string();
                let capabilities = item
                    .get("capabilities")
                    .and_then(|value| value.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str())
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| {
                        vec!["controller".to_string(), "general_response".to_string()]
                    });

                if id.is_empty()
                    || display_name.is_empty()
                    || base_url.is_empty()
                    || model.is_empty()
                {
                    return Err("ASSISTANT_LOCAL_MODELS entries require non-empty id, display_name, url, and model.".into());
                }

                if models.contains_key(&id) {
                    return Err(format!("Duplicate local model id: {id}").into());
                }

                models.insert(
                    id.clone(),
                    LocalModelDefinition {
                        id,
                        display_name,
                        base_url,
                        model,
                        capabilities,
                    },
                );
            }
        }

        Ok(Self {
            models: Arc::new(models),
            active_model: Arc::new(RwLock::new("qwen".to_string())),
        })
    }

    fn active_definition(&self) -> Result<LocalModelDefinition, Box<dyn Error>> {
        let active = self
            .active_model
            .read()
            .map_err(|_| "Active model state lock was poisoned.")?
            .clone();

        self.models
            .get(&active)
            .cloned()
            .ok_or_else(|| format!("Active model '{active}' is not registered.").into())
    }

    fn active_model_id(&self) -> Result<String, Box<dyn Error>> {
        Ok(self
            .active_model
            .read()
            .map_err(|_| "Active model state lock was poisoned.")?
            .clone())
    }

    fn switch_model(&self, model: &str) -> Result<ModelInfo, Box<dyn Error>> {
        let normalized = model.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err("Model id cannot be empty.".into());
        }

        let definition = self.models.get(&normalized).ok_or_else(|| {
            format!("Unknown local model '{normalized}'. Use :model list to see registered models.")
        })?;

        let available = self.probe_model(definition)?;
        if !available {
            return Err(format!(
                "Local model '{}' is unavailable at {}.",
                definition.display_name, definition.base_url
            )
            .into());
        }

        let mut active = self
            .active_model
            .write()
            .map_err(|_| "Active model state lock was poisoned.")?;
        *active = definition.id.clone();
        drop(active);

        self.model_info_with_availability(definition, available)
    }

    fn probe_model(&self, definition: &LocalModelDefinition) -> Result<bool, Box<dyn Error>> {
        let endpoint = format!("{}/v1/models", definition.base_url.trim_end_matches('/'));
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(MODEL_PROBE_CONNECT_TIMEOUT)
            .timeout(MODEL_PROBE_TIMEOUT)
            .build()?;

        match client.get(endpoint).send() {
            Ok(response) => Ok(response.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    fn model_info_with_availability(
        &self,
        definition: &LocalModelDefinition,
        available: bool,
    ) -> Result<ModelInfo, Box<dyn Error>> {
        let active = self.active_model_id()?;

        Ok(ModelInfo {
            id: definition.id.clone(),
            display_name: definition.display_name.clone(),
            available,
            active: active == definition.id,
            capabilities: definition.capabilities.clone(),
        })
    }

    fn model_info(&self, id: &str) -> Result<ModelInfo, Box<dyn Error>> {
        let definition = self
            .models
            .get(id)
            .ok_or_else(|| format!("Unknown local model '{id}'."))?;
        let available = self.probe_model(definition)?;
        Ok(self.model_info_with_availability(definition, available)?)
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error>> {
        let mut ids = self.models.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        ids.into_iter().map(|id| self.model_info(&id)).collect()
    }

    fn status(&self) -> Result<ModelState, Box<dyn Error>> {
        let active_model = self.active_model_id()?;
        let definition = self
            .models
            .get(&active_model)
            .ok_or_else(|| format!("Active model '{active_model}' is not registered."))?;
        let ready = self.probe_model(definition)?;

        Ok(ModelState {
            active_model: Some(active_model),
            ready,
        })
    }
}

impl Controller for ModelSession {
    fn plan(
        &self,
        context: &orchestrator::ControllerContext,
    ) -> Result<orchestrator::ControllerPlan, Box<dyn Error>> {
        let definition = self.active_definition()?;
        let controller = LlamaCppController::new(definition.base_url, definition.model);
        controller.plan(context)
    }
}

trait ModelControllerState {
    fn active_model_id(&self) -> Result<String, Box<dyn Error>>;
    fn switch_model(&self, model: &str) -> Result<ModelInfo, Box<dyn Error>>;
    fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error>>;
}

impl ModelControllerState for ModelSession {
    fn active_model_id(&self) -> Result<String, Box<dyn Error>> {
        ModelSession::active_model_id(self)
    }

    fn switch_model(&self, model: &str) -> Result<ModelInfo, Box<dyn Error>> {
        ModelSession::switch_model(self, model)
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error>> {
        ModelSession::list_models(self)
    }
}

struct RuntimeIpcHandler {
    model_session: ModelSession,
}

impl ipc::RequestHandler for RuntimeIpcHandler {
    fn handle(&self, request: WireRequest) -> WireResponse {
        let id = request.id;

        match request.method {
            RequestMethod::Ping => WireResponse::ok(id, ResponsePayload::Pong),
            RequestMethod::Health => WireResponse::ok(
                id,
                ResponsePayload::Health(HealthStatus {
                    runtime: env!("CARGO_PKG_NAME").to_string(),
                    ready: true,
                }),
            ),
            RequestMethod::ModelList => match self.model_session.list_models() {
                Ok(models) => WireResponse::ok(id, ResponsePayload::Models(ModelList { models })),
                Err(error) => WireResponse::error(id, "model_list_failed", error.to_string()),
            },
            RequestMethod::ModelStatus => match self.model_session.status() {
                Ok(status) => WireResponse::ok(id, ResponsePayload::ModelStatus(status)),
                Err(error) => WireResponse::error(id, "model_status_failed", error.to_string()),
            },
            RequestMethod::ModelSwitch { model } => match self.model_session.switch_model(&model) {
                Ok(_) => match self.model_session.status() {
                    Ok(status) => WireResponse::ok(id, ResponsePayload::ModelStatus(status)),
                    Err(error) => WireResponse::error(id, "model_status_failed", error.to_string()),
                },
                Err(error) => WireResponse::error(id, "model_switch_failed", error.to_string()),
            },
            _ => WireResponse::error(
                id,
                "unsupported",
                "This IPC method is not implemented by the runtime service yet.",
            ),
        }
    }
}

struct SessionController<C> {
    inner: C,
    db: SqliteMemoryDb,
    session_id: String,
}

impl<C> SessionController<C> {
    fn new(inner: C, db: SqliteMemoryDb) -> Self {
        Self {
            inner,
            db,
            session_id: Self::new_session_id(),
        }
    }

    fn new_session_id() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        format!("session-{}-{}", now.as_nanos(), std::process::id(),)
    }

    fn start_new_session(&mut self) {
        self.session_id = Self::new_session_id();
    }

    fn active_model_id(&self) -> Result<String, Box<dyn Error>>
    where
        C: ModelControllerState,
    {
        self.inner.active_model_id()
    }

    fn switch_model(&self, model: &str) -> Result<ModelInfo, Box<dyn Error>>
    where
        C: ModelControllerState,
    {
        self.inner.switch_model(model)
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error>>
    where
        C: ModelControllerState,
    {
        self.inner.list_models()
    }

    fn truncate(text: &str, limit: usize) -> String {
        text.chars().take(limit).collect()
    }

    fn conversation_history(&self) -> Result<String, Box<dyn Error>> {
        let history = self
            .db
            .recent_conversation_turns(&self.session_id, MAX_SESSION_TURNS)?;

        let mut selected = Vec::new();
        let mut used_chars = 0usize;

        for (user, assistant) in history.into_iter().rev() {
            let user = Self::truncate(&user, MAX_HISTORY_TURN_CHARS);
            let assistant = Self::truncate(&assistant, MAX_HISTORY_TURN_CHARS);

            let block = format!("USER:\n{}\n\nASSISTANT:\n{}", user, assistant);
            let block_chars = block.chars().count();
            let separator_chars = if selected.is_empty() { 0 } else { 2 };

            if used_chars + separator_chars + block_chars > MAX_HISTORY_CHARS {
                break;
            }

            selected.push(block);
            used_chars += separator_chars + block_chars;
        }

        selected.reverse();

        Ok(selected.join("\n\n"))
    }

    fn record_response(
        &self,
        request: &orchestrator::UserRequest,
        response: &str,
    ) -> Result<(), Box<dyn Error>> {
        let turn_id =
            self.db
                .append_conversation_turn(&self.session_id, &request.text, response)?;

        self.db.enqueue_job(
            "extract_memory",
            Some("conversation"),
            Some(&turn_id.to_string()),
            0,
            None,
        )?;

        Ok(())
    }
}

impl<C> Controller for SessionController<C>
where
    C: Controller,
{
    fn plan(
        &self,
        context: &orchestrator::ControllerContext,
    ) -> Result<orchestrator::ControllerPlan, Box<dyn Error>> {
        let contextual_context = orchestrator::ControllerContext {
            request: context.request.clone(),
            history: self.conversation_history()?,
            observations: context.observations.clone(),
        };

        self.inner.plan(&contextual_context)
    }
}

struct InteractiveApproval;

impl ApprovalHandler for InteractiveApproval {
    fn approve(&self, call: &ToolCall) -> Result<bool, Box<dyn Error>> {
        println!();
        println!("============================================================");
        println!("APPROVAL REQUIRED");
        println!("============================================================");
        println!("Tool: {}", call.tool);
        println!(
            "Arguments:\n{}",
            serde_json::to_string_pretty(&call.arguments)?
        );

        print!("Approve? [y/N]: ");
        io::stdout().flush()?;

        let mut input = String::new();

        io::stdin().read_line(&mut input)?;

        let approved = matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes");

        println!("{}", if approved { "Approved." } else { "Denied." });

        Ok(approved)
    }
}

fn env_or_default(name: &str, default: String) -> String {
    env::var(name).unwrap_or(default)
}

fn home_directory() -> Result<PathBuf, Box<dyn Error>> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME environment variable is not set.".into())
}

fn memory_root() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = env::var_os("ASSISTANT_MEMORY_ROOT") {
        return Ok(PathBuf::from(value));
    }

    Ok(home_directory()?.join("assistant-memory"))
}

fn file_root() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = env::var_os("ASSISTANT_FILE_ROOT") {
        return Ok(PathBuf::from(value));
    }

    Ok(env::current_dir()?)
}

fn active_tasks(
    records: Vec<sqlite_memory::TaskRecord>,
    status: Option<&str>,
) -> Vec<sqlite_memory::TaskRecord> {
    match status {
        Some("all") | Some("open") | Some("in_progress") => records,
        Some(_) => records,
        None => records
            .into_iter()
            .filter(|record| matches!(record.status.as_str(), "open" | "in_progress"))
            .collect(),
    }
}

fn active_reminders(
    records: Vec<sqlite_memory::ReminderRecord>,
    status: Option<&str>,
) -> Vec<sqlite_memory::ReminderRecord> {
    match status {
        Some("all") | Some("scheduled") | Some("triggered") => records,
        Some(_) => records,
        None => records
            .into_iter()
            .filter(|record| matches!(record.status.as_str(), "scheduled" | "triggered"))
            .collect(),
    }
}

fn print_tasks(db: &SqliteMemoryDb, status: Option<&str>) -> Result<(), Box<dyn Error>> {
    let records = active_tasks(db.tasks(status, 100)?, status);

    if records.is_empty() {
        println!("No matching tasks.");
        return Ok(());
    }

    println!();
    for task in records {
        let due = task.due_at.as_deref().unwrap_or("no due date");
        println!("{} | {} | {} | {}", task.id, task.status, due, task.title);
        if !task.body.trim().is_empty() {
            println!("  {}", task.body.trim().replace('\n', "\n  "));
        }
    }
    println!();

    Ok(())
}

fn print_reminders(db: &SqliteMemoryDb, status: Option<&str>) -> Result<(), Box<dyn Error>> {
    let records = active_reminders(db.reminders(status, 100)?, status);

    if records.is_empty() {
        println!("No matching reminders.");
        return Ok(());
    }

    println!();
    for reminder in records {
        let due = reminder.due_at.as_deref().unwrap_or("no due date");
        println!(
            "{} | {} | {} | {}",
            reminder.id, reminder.status, due, reminder.title
        );
        if !reminder.body.trim().is_empty() {
            println!("  {}", reminder.body.trim().replace('\n', "\n  "));
        }
    }
    println!();

    Ok(())
}

fn print_status(
    db: &SqliteMemoryDb,
    memory_root: &PathBuf,
    file_root: &PathBuf,
    db_path: &PathBuf,
    qwen_url: &str,
    qwen_model: &str,
    active_model: &str,
    embedding_url: &str,
    embedding_model: &str,
    background_workers_enabled: bool,
) -> Result<(), Box<dyn Error>> {
    let task_count = db.tasks(None, 1000)?.len();
    let reminder_count = db.reminders(None, 1000)?.len();
    let job_count = db.jobs(None, 1000)?.len();

    println!();
    println!("Assistant status: online");
    println!("Memory root: {}", memory_root.display());
    println!("File root: {}", file_root.display());
    println!("SQLite DB: {}", db_path.display());
    println!("Qwen backend: {} ({})", qwen_model, qwen_url);
    println!("Active controller model: {}", active_model);
    println!("Embeddings: {} ({})", embedding_model, embedding_url);
    println!(
        "Background workers: {}",
        if background_workers_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("Tasks indexed: {}", task_count);
    println!("Reminders indexed: {}", reminder_count);
    println!("Jobs indexed: {}", job_count);
    println!();
    Ok(())
}

fn print_jobs(db: &SqliteMemoryDb, status: Option<&str>) -> Result<(), Box<dyn Error>> {
    let jobs = db.jobs(status, 100)?;
    if jobs.is_empty() {
        println!("No matching jobs.");
        return Ok(());
    }

    println!();
    for job in jobs {
        let status = match job.status {
            sqlite_memory::JobStatus::Queued => "queued",
            sqlite_memory::JobStatus::Running => "running",
            sqlite_memory::JobStatus::Completed => "completed",
            sqlite_memory::JobStatus::Failed => "failed",
            sqlite_memory::JobStatus::Cancelled => "cancelled",
        };
        let target = match (job.target_type.as_deref(), job.target_id.as_deref()) {
            (Some(kind), Some(id)) => format!("{}:{}", kind, id),
            _ => "-".to_string(),
        };
        println!(
            "{} | {} | {} | attempts={} | target={} | next_run={}",
            job.id, status, job.job_type, job.attempts, target, job.next_run_at
        );
        if let Some(error) = job.last_error.as_deref() {
            println!("  error: {}", error);
        }
    }
    println!();
    Ok(())
}

fn parse_model_switch_command(text: &str) -> Option<String> {
    let cleaned = text.trim().trim_end_matches(['.', '!', '?']).trim();
    let lower = cleaned.to_ascii_lowercase();

    for prefix in [
        "switch to ",
        "switch model to ",
        "change model to ",
        "use model ",
        "use ",
    ] {
        if let Some(remainder) = lower.strip_prefix(prefix) {
            let model = cleaned[prefix.len()..].trim();
            if !remainder.trim().is_empty() && !model.is_empty() {
                return Some(model.to_ascii_lowercase());
            }
        }
    }

    None
}

#[derive(Debug, PartialEq, Eq)]
enum ClientCommand {
    Ping,
    Health,
    ModelList,
    ModelStatus,
    ModelSwitch(String),
    Quit,
}

fn parse_client_command(text: &str) -> Result<ClientCommand, String> {
    let text = text.trim();
    match text {
        ":quit" | ":exit" => Ok(ClientCommand::Quit),
        ":ping" => Ok(ClientCommand::Ping),
        ":health" => Ok(ClientCommand::Health),
        ":model" | ":model list" => Ok(ClientCommand::ModelList),
        ":model status" => Ok(ClientCommand::ModelStatus),
        _ if text.starts_with(":model use ") => {
            let model = text.strip_prefix(":model use ").unwrap_or("").trim();
            if model.is_empty() {
                Err("Usage: :model use <id>".to_string())
            } else {
                Ok(ClientCommand::ModelSwitch(model.to_ascii_lowercase()))
            }
        }
        _ => Err(
            "Client mode commands: :ping, :health, :model [list|status|use <id>], :quit"
                .to_string(),
        ),
    }
}

fn print_client_response(response: ResponsePayload) {
    match response {
        ResponsePayload::Pong => println!("Pong."),
        ResponsePayload::Health(status) => {
            println!("Runtime: {} | ready={}", status.runtime, status.ready);
        }
        ResponsePayload::Models(model_list) => {
            println!();
            for model in model_list.models {
                let state = if model.active {
                    "active"
                } else if model.available {
                    "available"
                } else {
                    "unavailable"
                };
                println!(
                    "{} | {} | {}{}",
                    model.id,
                    state,
                    model.display_name,
                    if model.capabilities.is_empty() {
                        String::new()
                    } else {
                        format!(" | {}", model.capabilities.join(","))
                    }
                );
            }
            println!();
        }
        ResponsePayload::ModelStatus(status) => {
            println!(
                "Active controller model: {} | ready={}",
                status.active_model.as_deref().unwrap_or("none"),
                status.ready
            );
        }
        other => println!("Unexpected client response: {other:?}"),
    }
}

fn run_client_mode() -> Result<(), Box<dyn Error>> {
    let socket_path = ipc::default_socket_path()?;
    let client = assistant_client::IpcClient::new(&socket_path);

    println!("============================================================");
    println!("PERSONAL ASSISTANT CLIENT");
    println!("============================================================");
    println!("Runtime socket: {}", socket_path.display());
    println!("Commands: :ping, :health, :model [list|status|use <id>], :quit");
    println!("============================================================");
    println!();

    loop {
        print!("assistant-client> ");
        io::stdout().flush()?;

        let mut input = String::new();
        let bytes = io::stdin().read_line(&mut input)?;
        if bytes == 0 {
            println!();
            break;
        }

        let text = input.trim();
        if text.is_empty() {
            continue;
        }

        let command = match parse_client_command(text) {
            Ok(command) => command,
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
        };

        if command == ClientCommand::Quit {
            break;
        }

        let method = match command {
            ClientCommand::Ping => RequestMethod::Ping,
            ClientCommand::Health => RequestMethod::Health,
            ClientCommand::ModelList => RequestMethod::ModelList,
            ClientCommand::ModelStatus => RequestMethod::ModelStatus,
            ClientCommand::ModelSwitch(model) => RequestMethod::ModelSwitch { model },
            ClientCommand::Quit => unreachable!(),
        };

        match client.request(method) {
            Ok(response) => print_client_response(response),
            Err(error) => eprintln!("Client request failed: {error}"),
        }
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let daemon_mode = arguments.iter().any(|argument| argument == "--daemon");
    let client_mode = arguments.iter().any(|argument| argument == "--client");

    if daemon_mode && client_mode {
        return Err("--daemon and --client cannot be used together.".into());
    }

    if client_mode {
        return run_client_mode();
    }

    let memory_root = memory_root()?;

    std::fs::create_dir_all(&memory_root)?;

    let file_root = file_root()?;

    let db_path = PathBuf::from(env_or_default(
        "ASSISTANT_SQLITE_DB_PATH",
        memory_root
            .join("assistant.db")
            .to_string_lossy()
            .into_owned(),
    ));

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let qwen_url = env_or_default("ASSISTANT_QWEN_URL", "http://127.0.0.1:8080".to_string());

    let qwen_model = env_or_default("ASSISTANT_QWEN_MODEL", "Qwen3.5-4B-Q4_K_M.gguf".to_string());

    let embedding_url = env_or_default(
        "ASSISTANT_EMBEDDING_URL",
        "http://127.0.0.1:8081".to_string(),
    );

    let embedding_model = env_or_default(
        "ASSISTANT_EMBEDDING_MODEL",
        "bge-small-en-v1.5-q8_0.gguf".to_string(),
    );

    println!("============================================================");
    println!("PERSONAL ASSISTANT");
    println!("============================================================");
    println!("Memory root:  {}", memory_root.display());
    println!("File root:    {}", file_root.display());
    println!("SQLite DB:    {}", db_path.display());
    println!("Qwen:         {}", qwen_url);
    println!("Embeddings:   {}", embedding_url);
    println!("============================================================");

    let indexer = PersistentMemoryIndexer::new(
        &db_path,
        &memory_root,
        embedding_url.clone(),
        embedding_model.clone(),
    )?;

    println!();
    println!("Refreshing persistent memory index...");

    indexer.refresh()?;

    println!("Memory index: ready.");

    let conversation_db = SqliteMemoryDb::open(&db_path)?;

    conversation_db.initialize_schema()?;

    let embedding_url_for_display = embedding_url.clone();
    let embedding_model_for_display = embedding_model.clone();
    let memory = HybridMemory::open(&db_path, embedding_url, embedding_model)?;

    let mut registry = tools::ToolRegistry::new();

    registry.register(tools::FileReadTool::new(&file_root));

    registry.register(tools::FileListTool::new(&file_root));

    registry.register(tools::MemoryWriteTool::new(&memory_root));

    registry.register(tools::TaskListTool::new(&db_path)?);
    registry.register(tools::TaskMutationTool::new(&memory_root, &db_path)?);
    registry.register(tools::ReminderListTool::new(&db_path)?);
    registry.register(tools::ReminderMutationTool::new(&memory_root, &db_path)?);

    let google_calendar_id = env_or_default("ASSISTANT_GOOGLE_CALENDAR_ID", "primary".to_string());
    let google_calendar_token = env::var("ASSISTANT_GOOGLE_CALENDAR_ACCESS_TOKEN").ok();
    let google_calendar_auth = tools::GoogleCalendarAuth::from_env()?;
    let calendar_uses_static_token = google_calendar_token.is_some();

    if let Some(access_token) = google_calendar_token {
        registry.register(tools::GoogleCalendarListEventsTool::with_credentials(
            access_token.clone(),
            google_calendar_id.clone(),
        )?);
        registry.register(tools::GoogleCalendarGetEventTool::with_credentials(
            access_token.clone(),
            google_calendar_id.clone(),
        )?);
        registry.register(tools::GoogleCalendarCreateEventTool::with_credentials(
            access_token.clone(),
            google_calendar_id.clone(),
        )?);
        registry.register(tools::GoogleCalendarUpdateEventTool::with_credentials(
            access_token.clone(),
            google_calendar_id.clone(),
        )?);
        registry.register(tools::GoogleCalendarDeleteEventTool::with_credentials(
            access_token,
            google_calendar_id.clone(),
        )?);
        println!(
            "Google Calendar tools enabled via access-token environment variable. Write operations still require interactive approval."
        );
    } else if google_calendar_auth.is_authenticated() || google_calendar_auth.has_client_id() {
        registry.register(tools::GoogleCalendarListEventsTool::with_auth(
            google_calendar_auth.clone(),
            google_calendar_id.clone(),
        )?);
        registry.register(tools::GoogleCalendarGetEventTool::with_auth(
            google_calendar_auth.clone(),
            google_calendar_id.clone(),
        )?);
        registry.register(tools::GoogleCalendarCreateEventTool::with_auth(
            google_calendar_auth.clone(),
            google_calendar_id.clone(),
        )?);
        registry.register(tools::GoogleCalendarUpdateEventTool::with_auth(
            google_calendar_auth.clone(),
            google_calendar_id.clone(),
        )?);
        registry.register(tools::GoogleCalendarDeleteEventTool::with_auth(
            google_calendar_auth.clone(),
            google_calendar_id,
        )?);

        if google_calendar_auth.is_authenticated() {
            if google_calendar_auth.has_event_write_scope()? {
                println!(
                    "Google Calendar: read/write tools enabled with persistent OAuth credentials ({}).",
                    google_calendar_auth.token_path().display()
                );
            } else {
                println!(
                    "Google Calendar: authenticated with read-only credentials. Run :calendar-login again to enable event mutations ({}).",
                    google_calendar_auth.token_path().display()
                );
            }
        } else {
            println!(
                "Google Calendar: configured for OAuth but not authenticated. Use :calendar-login."
            );
        }
    } else {
        println!(
            "Google Calendar: not configured. Set ASSISTANT_GOOGLE_CLIENT_ID to enable OAuth, or ASSISTANT_GOOGLE_CALENDAR_ACCESS_TOKEN for a temporary token."
        );
    }

    let tool_executor = IndexedToolExecutor::new(registry, indexer.clone());

    let model_session = ModelSession::from_environment(&qwen_url, &qwen_model)?;

    let controller = SessionController::new(model_session.clone(), conversation_db);

    let mut models = ModelRouter::new(qwen_url.clone(), qwen_model.clone());

    if let Ok(api_key) = env::var("ASSISTANT_CLAUDE_API_KEY") {
        if api_key.trim().is_empty() {
            return Err("ASSISTANT_CLAUDE_API_KEY cannot be empty when set.".into());
        }
        let model = env_or_default("ASSISTANT_CLAUDE_MODEL", "claude-sonnet-5".to_string());
        let base_url = env_or_default(
            "ASSISTANT_CLAUDE_URL",
            "https://api.anthropic.com".to_string(),
        );
        models.register_claude_with_base_url(api_key, model.clone(), base_url)?;
        println!("Claude model backend: {model}");
    }

    if let Ok(api_key) = env::var("ASSISTANT_CODEX_API_KEY") {
        if api_key.trim().is_empty() {
            return Err("ASSISTANT_CODEX_API_KEY cannot be empty when set.".into());
        }
        let model = env_or_default("ASSISTANT_CODEX_MODEL", "gpt-5.3-codex".to_string());
        let base_url = env_or_default("ASSISTANT_CODEX_URL", "https://api.openai.com".to_string());
        models.register_codex_with_base_url(api_key, model.clone(), base_url)?;
        println!("Codex model backend: {model}");
    }

    let background_workers_enabled = env::var("ASSISTANT_BACKGROUND_WORKERS")
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no"
            )
        })
        .unwrap_or(true);

    let mut background_workers = if background_workers_enabled {
        match sqlite_ingest::BackgroundWorkers::start(
            &memory_root,
            &db_path,
            qwen_url.clone(),
            qwen_model.clone(),
        ) {
            Ok(workers) => {
                println!("Background workers: memory extraction + reminder scheduler enabled.");
                Some(workers)
            }
            Err(error) => {
                eprintln!("Background workers could not start: {error}");
                None
            }
        }
    } else {
        println!("Background workers: disabled via ASSISTANT_BACKGROUND_WORKERS.");
        None
    };

    let mut orchestrator = Orchestrator {
        controller,
        memory,
        tools: tool_executor,
        models,
        approvals: Box::new(InteractiveApproval),
    };

    println!();
    println!("Ready.");
    println!(
        "Commands: :quit, :exit, :new, :status, :health, :model [list|status|use <id>], :jobs [status], :tasks [status], :reminders [status], :memory-proposals, :memory-accept <id>, :memory-reject <id>"
    );
    println!();

    if daemon_mode {
        let handler = Arc::new(RuntimeIpcHandler {
            model_session: model_session.clone(),
        });
        let socket_path = ipc::default_socket_path()?;
        ipc::serve(&socket_path, handler)
            .map_err(|error| std::io::Error::other(error.to_string()))?;

        if let Some(workers) = background_workers.as_mut() {
            workers.shutdown();
        }

        return Ok(());
    }

    loop {
        print!("assistant> ");
        io::stdout().flush()?;

        let mut input = String::new();

        let bytes = io::stdin().read_line(&mut input)?;

        if bytes == 0 {
            println!();
            break;
        }

        let text = input.trim();

        if text.is_empty() {
            continue;
        }

        if matches!(text, ":new") {
            orchestrator.controller.start_new_session();

            println!();
            println!("Started a new conversation session.");
            println!();

            continue;
        }

        if text == ":model" || text.starts_with(":model ") {
            let argument = text.strip_prefix(":model").unwrap_or("").trim();

            match argument {
                "" | "list" => match orchestrator.controller.list_models() {
                    Ok(models) => {
                        println!();
                        for model in models {
                            println!(
                                "{} | {} | {}{}",
                                model.id,
                                if model.active { "active" } else { "available" },
                                model.display_name,
                                if model.capabilities.is_empty() {
                                    String::new()
                                } else {
                                    format!(" | {}", model.capabilities.join(","))
                                }
                            );
                        }
                        println!();
                    }
                    Err(error) => eprintln!("Could not list models: {error}"),
                },
                "status" => match orchestrator.controller.active_model_id() {
                    Ok(model) => println!("Active controller model: {model}"),
                    Err(error) => eprintln!("Could not read active model: {error}"),
                },
                value if value.starts_with("use ") => {
                    let model = value.strip_prefix("use ").unwrap_or("").trim();
                    match orchestrator.controller.switch_model(model) {
                        Ok(model) => println!(
                            "Switched controller to {} ({}).",
                            model.id, model.display_name
                        ),
                        Err(error) => eprintln!("Could not switch model: {error}"),
                    }
                }
                _ => eprintln!("Usage: :model [list|status|use <id>]"),
            }
            continue;
        }

        if matches!(text, ":status" | ":health") {
            let db = SqliteMemoryDb::open(&db_path)?;
            print_status(
                &db,
                &memory_root,
                &file_root,
                &db_path,
                &qwen_url,
                &qwen_model,
                &orchestrator.controller.active_model_id()?,
                &embedding_url_for_display,
                &embedding_model_for_display,
                background_workers_enabled,
            )?;
            continue;
        }

        if text == ":jobs" || text.starts_with(":jobs ") {
            let argument = text.strip_prefix(":jobs").unwrap_or("").trim();
            if argument.contains(char::is_whitespace) {
                eprintln!("Usage: :jobs [status]");
                continue;
            }
            let status = match argument {
                "" | "status" => None,
                other => Some(other),
            };
            let db = SqliteMemoryDb::open(&db_path)?;
            if let Err(error) = print_jobs(&db, status) {
                eprintln!("Could not list jobs: {}", error);
            }
            continue;
        }

        if text == ":tasks" || text.starts_with(":tasks ") {
            let argument = text.strip_prefix(":tasks").unwrap_or("").trim();

            if argument.contains(char::is_whitespace) {
                eprintln!("Usage: :tasks [status]");
                continue;
            }

            let status = if argument.is_empty() {
                None
            } else {
                Some(argument)
            };

            let db = SqliteMemoryDb::open(&db_path)?;
            if let Err(error) = print_tasks(&db, status) {
                eprintln!("Could not list tasks: {}", error);
            }

            continue;
        }

        if text == ":reminders" || text.starts_with(":reminders ") {
            let argument = text.strip_prefix(":reminders").unwrap_or("").trim();

            if argument.contains(char::is_whitespace) {
                eprintln!("Usage: :reminders [status]");
                continue;
            }

            let status = if argument.is_empty() {
                None
            } else {
                Some(argument)
            };

            let db = SqliteMemoryDb::open(&db_path)?;
            if let Err(error) = print_reminders(&db, status) {
                eprintln!("Could not list reminders: {}", error);
            }

            continue;
        }

        if matches!(text, ":memory-proposals") {
            match indexer.pending_memory_extraction_proposals(20) {
                Ok(proposals) if proposals.is_empty() => println!("No pending memory proposals."),
                Ok(proposals) => {
                    println!();
                    for proposal in proposals {
                        println!(
                            "{} | {} | turn {} | {}",
                            proposal.id,
                            proposal.decision,
                            proposal.conversation_turn_id,
                            proposal.created_at
                        );
                        if let Ok(payload) =
                            serde_json::from_str::<serde_json::Value>(&proposal.payload_json)
                        {
                            if let Some(items) =
                                payload.get("items").and_then(|value| value.as_array())
                            {
                                for item in items.iter().take(4) {
                                    let title = item
                                        .get("title")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("(untitled)");
                                    let body = item
                                        .get("body")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("");
                                    let excerpt: String = body.chars().take(180).collect();
                                    println!("  - {}: {}", title, excerpt);
                                }
                            }
                        }
                    }
                    println!();
                }
                Err(error) => eprintln!("Could not list memory proposals: {}", error),
            }
            continue;
        }

        if let Some(proposal_id) = text.strip_prefix(":memory-accept ") {
            let proposal_id = proposal_id.trim();
            if proposal_id.is_empty() {
                eprintln!("Usage: :memory-accept <proposal-id>");
                continue;
            }
            match indexer.apply_memory_extraction_proposal(proposal_id) {
                Ok(paths) if paths.is_empty() => println!("Memory proposal was already applied."),
                Ok(paths) => {
                    println!("Applied memory proposal.");
                    for path in paths {
                        println!("  {}", path);
                    }
                }
                Err(error) => eprintln!("Could not apply memory proposal: {}", error),
            }
            continue;
        }

        if let Some(proposal_id) = text.strip_prefix(":memory-reject ") {
            let proposal_id = proposal_id.trim();
            if proposal_id.is_empty() {
                eprintln!("Usage: :memory-reject <proposal-id>");
                continue;
            }
            match indexer.reject_memory_extraction_proposal(proposal_id) {
                Ok(true) => println!("Rejected memory proposal."),
                Ok(false) => println!("Memory proposal was already handled."),
                Err(error) => eprintln!("Could not reject memory proposal: {}", error),
            }
            continue;
        }

        if matches!(text, ":calendar-login") {
            if calendar_uses_static_token {
                eprintln!(
                    "Google Calendar is currently using ASSISTANT_GOOGLE_CALENDAR_ACCESS_TOKEN. Unset that variable and restart the assistant before using :calendar-login."
                );
            } else {
                match google_calendar_auth.login() {
                    Ok(()) => {
                        println!("Google Calendar: authentication complete.");
                    }
                    Err(error) => {
                        eprintln!("Google Calendar login failed: {}", error);
                    }
                }
            }

            continue;
        }

        if let Some(model) = parse_model_switch_command(text) {
            let matched_id = orchestrator
                .controller
                .list_models()?
                .into_iter()
                .find(|entry| entry.id == model || entry.display_name.to_ascii_lowercase() == model)
                .map(|entry| entry.id);

            if let Some(model_id) = matched_id {
                match orchestrator.controller.switch_model(&model_id) {
                    Ok(info) => {
                        println!(
                            "Switched controller to {} ({}).",
                            info.id, info.display_name
                        );
                        continue;
                    }
                    Err(error) => eprintln!("Could not switch model: {error}"),
                }
            }
        }

        let request = orchestrator::UserRequest {
            text: text.to_string(),
        };

        match orchestrator.run(request.clone()) {
            Ok(response) => {
                orchestrator
                    .controller
                    .record_response(&request, &response)?;

                println!();
                println!("{}", response);
                println!();
            }

            Err(error) => {
                eprintln!();
                eprintln!("Assistant error: {}", error);
                eprintln!();
            }
        }
    }

    if let Some(workers) = background_workers.as_mut() {
        workers.shutdown();
    }

    println!("Goodbye.");

    Ok(())
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use std::{io::Read, net::TcpListener, thread};

    struct FakeController {
        contexts: std::sync::Mutex<Vec<orchestrator::ControllerContext>>,
    }

    impl Controller for FakeController {
        fn plan(
            &self,
            context: &orchestrator::ControllerContext,
        ) -> Result<orchestrator::ControllerPlan, Box<dyn Error>> {
            let mut contexts = self
                .contexts
                .lock()
                .map_err(|_| "Fake controller mutex was poisoned.")?;

            contexts.push(context.clone());

            Ok(orchestrator::ControllerPlan {
                reasoning_summary: "Respond.".to_string(),
                actions: vec![orchestrator::ControllerAction::Respond {
                    text: "Test response.".to_string(),
                }],
            })
        }
    }

    fn test_database() -> Result<(tempfile::TempDir, SqliteMemoryDb), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;

        let database = directory.path().join("session.db");

        let db = SqliteMemoryDb::open(&database)?;

        db.initialize_schema()?;

        Ok((directory, db))
    }

    fn task_record(id: &str, status: &str) -> sqlite_memory::TaskRecord {
        sqlite_memory::TaskRecord {
            id: id.to_string(),
            note_id: id.to_string(),
            title: id.to_string(),
            body: String::new(),
            status: status.to_string(),
            due_at: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn reminder_record(id: &str, status: &str) -> sqlite_memory::ReminderRecord {
        sqlite_memory::ReminderRecord {
            id: id.to_string(),
            note_id: id.to_string(),
            title: id.to_string(),
            body: String::new(),
            status: status.to_string(),
            due_at: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn client_command_parser_handles_runtime_commands() {
        assert_eq!(parse_client_command(":ping"), Ok(ClientCommand::Ping));
        assert_eq!(parse_client_command(":health"), Ok(ClientCommand::Health));
        assert_eq!(parse_client_command(":model"), Ok(ClientCommand::ModelList));
        assert_eq!(
            parse_client_command(":model status"),
            Ok(ClientCommand::ModelStatus)
        );
        assert_eq!(
            parse_client_command(":model use Qwen"),
            Ok(ClientCommand::ModelSwitch("qwen".to_string()))
        );
        assert_eq!(parse_client_command(":exit"), Ok(ClientCommand::Quit));
    }

    #[test]
    fn client_command_parser_rejects_unrecognized_input() {
        assert!(parse_client_command("hello").is_err());
        assert!(parse_client_command(":model switch qwen").is_err());
    }

    #[test]
    fn default_task_command_filters_to_active_statuses() {
        let active = active_tasks(
            vec![
                task_record("open", "open"),
                task_record("progress", "in_progress"),
                task_record("done", "completed"),
            ],
            None,
        );

        assert_eq!(active.len(), 2);
        assert!(
            active
                .iter()
                .all(|task| matches!(task.status.as_str(), "open" | "in_progress"))
        );
    }

    #[test]
    fn default_reminder_command_filters_to_active_statuses() {
        let active = active_reminders(
            vec![
                reminder_record("scheduled", "scheduled"),
                reminder_record("triggered", "triggered"),
                reminder_record("done", "completed"),
            ],
            None,
        );

        assert_eq!(active.len(), 2);
        assert!(
            active
                .iter()
                .all(|reminder| matches!(reminder.status.as_str(), "scheduled" | "triggered"))
        );
    }

    #[test]
    fn status_command_uses_unfiltered_task_and_reminder_queries() -> Result<(), Box<dyn Error>> {
        let (_directory, db) = test_database()?;

        print_status(
            &db,
            &PathBuf::from("/tmp/memory"),
            &PathBuf::from("/tmp/files"),
            &PathBuf::from("/tmp/assistant.db"),
            "http://127.0.0.1:8080",
            "test-qwen",
            "qwen",
            "http://127.0.0.1:8081",
            "test-embedding",
            false,
        )?;

        Ok(())
    }

    #[test]
    fn jobs_command_status_keyword_means_unfiltered_query() -> Result<(), Box<dyn Error>> {
        let (_directory, db) = test_database()?;

        db.enqueue_job("test_job", None, None, 0, None)?;

        let jobs = db.jobs(None, 100)?;

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_type, "test_job");

        Ok(())
    }

    #[test]
    fn plan_does_not_record_history() -> Result<(), Box<dyn Error>> {
        let (_directory, db) = test_database()?;

        let controller = SessionController::new(
            FakeController {
                contexts: std::sync::Mutex::new(Vec::new()),
            },
            db,
        );

        controller.plan(&orchestrator::ControllerContext {
            request: orchestrator::UserRequest {
                text: "First turn.".to_string(),
            },
            history: String::new(),
            observations: Vec::new(),
        })?;

        let turns = controller
            .db
            .recent_conversation_turns(&controller.session_id, MAX_SESSION_TURNS)?;

        assert!(turns.is_empty());

        Ok(())
    }

    #[test]
    fn record_response_enqueues_memory_extraction() -> Result<(), Box<dyn Error>> {
        let (_directory, db) = test_database()?;
        let controller = SessionController::new(
            FakeController {
                contexts: std::sync::Mutex::new(Vec::new()),
            },
            db,
        );
        let request = orchestrator::UserRequest {
            text: "Remember Qwen as my local model.".to_string(),
        };
        controller.record_response(&request, "Saved.")?;
        let job = controller
            .db
            .claim_next_job("2999-01-01T00:00:00Z")?
            .ok_or("expected extraction job")?;
        assert_eq!(job.job_type, "extract_memory");
        assert_eq!(job.target_type.as_deref(), Some("conversation"));
        assert_eq!(job.target_id.as_deref(), Some("1"));
        Ok(())
    }

    #[test]
    fn session_history_is_retained() -> Result<(), Box<dyn Error>> {
        let (_directory, db) = test_database()?;

        let controller = SessionController::new(
            FakeController {
                contexts: std::sync::Mutex::new(Vec::new()),
            },
            db,
        );

        let first = orchestrator::UserRequest {
            text: "My favorite local model is Qwen3.5 4B.".to_string(),
        };

        controller.record_response(&first, "The local model is Qwen3.5 4B.")?;

        controller.plan(&orchestrator::ControllerContext {
            request: orchestrator::UserRequest {
                text: "What did I just tell you?".to_string(),
            },
            history: String::new(),
            observations: Vec::new(),
        })?;

        let contexts = controller
            .inner
            .contexts
            .lock()
            .map_err(|_| "Fake controller mutex was poisoned.")?;

        assert_eq!(contexts.len(), 1);

        assert_eq!(contexts[0].request.text, "What did I just tell you?");

        assert!(
            contexts[0]
                .history
                .contains("My favorite local model is Qwen3.5 4B.")
        );

        assert!(
            contexts[0]
                .history
                .contains("The local model is Qwen3.5 4B.")
        );

        Ok(())
    }

    #[test]
    fn session_history_context_is_bounded() -> Result<(), Box<dyn Error>> {
        let (_directory, db) = test_database()?;

        let controller = SessionController::new(
            FakeController {
                contexts: std::sync::Mutex::new(Vec::new()),
            },
            db,
        );

        for index in 0..12 {
            controller.record_response(
                &orchestrator::UserRequest {
                    text: format!("Turn {index} {}", "u".repeat(1500)),
                },
                &format!("Response {index} {}", "a".repeat(1500)),
            )?;
        }

        controller.plan(&orchestrator::ControllerContext {
            request: orchestrator::UserRequest {
                text: "Current request.".to_string(),
            },
            history: String::new(),
            observations: Vec::new(),
        })?;

        let contexts = controller
            .inner
            .contexts
            .lock()
            .map_err(|_| "Fake controller mutex was poisoned.")?;

        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].history.chars().count() <= MAX_HISTORY_CHARS);
        assert!(contexts[0].history.contains("Turn 11"));
        assert!(contexts[0].history.contains("Response 11"));

        Ok(())
    }

    #[test]
    fn session_context_is_bounded() -> Result<(), Box<dyn Error>> {
        let (_directory, db) = test_database()?;

        let controller = SessionController::new(
            FakeController {
                contexts: std::sync::Mutex::new(Vec::new()),
            },
            db,
        );

        for index in 0..12 {
            controller.record_response(
                &orchestrator::UserRequest {
                    text: format!("Turn {index}"),
                },
                &format!("Response {index}"),
            )?;
        }

        let history = controller
            .db
            .recent_conversation_turns(&controller.session_id, MAX_SESSION_TURNS)?;

        assert_eq!(history.len(), MAX_SESSION_TURNS);

        assert_eq!(history[0].0, "Turn 4");

        assert_eq!(history[0].1, "Response 4");

        assert_eq!(history[MAX_SESSION_TURNS - 1].0, "Turn 11");

        assert_eq!(history[MAX_SESSION_TURNS - 1].1, "Response 11");

        Ok(())
    }

    #[test]
    fn model_switch_command_parses_explicit_switches() {
        assert_eq!(
            parse_model_switch_command("switch to gemma"),
            Some("gemma".to_string())
        );
        assert_eq!(
            parse_model_switch_command("Use model qwen."),
            Some("qwen".to_string())
        );
        assert_eq!(
            parse_model_switch_command("change model to phi"),
            Some("phi".to_string())
        );
    }

    #[test]
    fn model_switch_command_ignores_unrelated_text() {
        assert_eq!(parse_model_switch_command("summarize this note"), None);
        assert_eq!(parse_model_switch_command(":model use qwen"), None);
    }

    fn spawn_ok_model_server() -> Result<(String, thread::JoinHandle<()>), Box<dyn Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let body = r#"{"object":"list","data":[]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });

        Ok((format!("http://{address}"), handle))
    }

    fn unavailable_model_url() -> Result<String, Box<dyn Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);
        Ok(format!("http://{address}"))
    }

    #[test]
    fn model_list_reports_backend_availability() -> Result<(), Box<dyn Error>> {
        let (available_url, server) = spawn_ok_model_server()?;
        let unavailable_url = unavailable_model_url()?;

        let mut models = HashMap::new();
        models.insert(
            "qwen".to_string(),
            LocalModelDefinition {
                id: "qwen".to_string(),
                display_name: "Qwen3.5 4B".to_string(),
                base_url: unavailable_url,
                model: "qwen.gguf".to_string(),
                capabilities: vec!["controller".to_string()],
            },
        );
        models.insert(
            "gemma".to_string(),
            LocalModelDefinition {
                id: "gemma".to_string(),
                display_name: "Gemma 4 E4B".to_string(),
                base_url: available_url,
                model: "gemma.gguf".to_string(),
                capabilities: vec!["controller".to_string()],
            },
        );

        let session = ModelSession {
            models: Arc::new(models),
            active_model: Arc::new(RwLock::new("qwen".to_string())),
        };

        let models = session.list_models()?;
        let gemma = models.iter().find(|model| model.id == "gemma").unwrap();
        let qwen = models.iter().find(|model| model.id == "qwen").unwrap();

        assert!(gemma.available);
        assert!(!qwen.available);
        assert!(qwen.active);
        assert!(!gemma.active);

        server
            .join()
            .map_err(|_| "Model probe test server panicked.")?;
        Ok(())
    }

    #[test]
    fn model_session_switch_rejects_unavailable_models() -> Result<(), Box<dyn Error>> {
        let (available_url, server) = spawn_ok_model_server()?;
        let unavailable_url = unavailable_model_url()?;

        let mut models = HashMap::new();
        models.insert(
            "qwen".to_string(),
            LocalModelDefinition {
                id: "qwen".to_string(),
                display_name: "Qwen3.5 4B".to_string(),
                base_url: unavailable_url.clone(),
                model: "qwen.gguf".to_string(),
                capabilities: vec!["controller".to_string()],
            },
        );
        models.insert(
            "gemma".to_string(),
            LocalModelDefinition {
                id: "gemma".to_string(),
                display_name: "Gemma 4 E4B".to_string(),
                base_url: available_url,
                model: "gemma.gguf".to_string(),
                capabilities: vec!["controller".to_string()],
            },
        );

        let session = ModelSession {
            models: Arc::new(models),
            active_model: Arc::new(RwLock::new("qwen".to_string())),
        };

        let selected = session.switch_model("gemma")?;
        assert_eq!(selected.id, "gemma");
        assert!(selected.available);
        assert!(selected.active);
        assert_eq!(session.active_model_id()?, "gemma");

        let error = session.switch_model("qwen").unwrap_err();
        assert!(error.to_string().contains("unavailable"));
        assert_eq!(session.active_model_id()?, "gemma");

        server
            .join()
            .map_err(|_| "Model probe test server panicked.")?;
        Ok(())
    }
}
