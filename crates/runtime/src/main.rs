use model_router::ModelRouter;
use orchestrator::{
    ApprovalHandler, Controller, HybridMemory, IndexedToolExecutor, LlamaCppController,
    Orchestrator, PersistentMemoryIndexer, ToolCall,
};
use sqlite_memory::SqliteMemoryDb;
use std::{
    env,
    error::Error,
    io::{self, Write},
    path::PathBuf,
};

const MAX_SESSION_TURNS: usize = 8;
const MAX_HISTORY_CHARS: usize = 5000;
const MAX_HISTORY_TURN_CHARS: usize = 1200;

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
    println!("Qwen: {} ({})", qwen_model, qwen_url);
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

fn main() -> Result<(), Box<dyn Error>> {
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

    let controller = SessionController::new(
        LlamaCppController::new(qwen_url.clone(), qwen_model.clone()),
        conversation_db,
    );

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
        "Commands: :quit, :exit, :new, :status, :health, :jobs [status], :tasks [status], :reminders [status], :memory-proposals, :memory-accept <id>, :memory-reject <id>"
    );
    println!();

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

        if matches!(text, ":status" | ":health") {
            let db = SqliteMemoryDb::open(&db_path)?;
            print_status(
                &db,
                &memory_root,
                &file_root,
                &db_path,
                &qwen_url,
                &qwen_model,
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

        if matches!(text, ":quit" | ":exit") {
            break;
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
}
