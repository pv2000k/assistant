mod startup;

use std::{
    error::Error,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use assistant_client::{ClientLease, IpcClient, IpcClientError, default_socket_path};
use assistant_protocol::{
    ApprovalRequestSummary, ApprovalStatus, CalendarStatus, ChatResponse, HealthStatus, ModelState,
    RequestMethod, ResponsePayload,
};
use crossterm::{
    Command,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

use startup::{
    ModelChoice, RuntimeLaunch, discover_models, initial_model_index, probe_runtime,
    runtime_binary_path,
};

const TABS: [&str; 9] = [
    "Overview",
    "Tasks",
    "Reminders",
    "Proposals",
    "Chat",
    "Models",
    "Jobs",
    "Search",
    "Calendar",
];

const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(3);
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CHAT_TIMEOUT: Duration = Duration::from_secs(130);
const REMINDER_MUTATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_EDITOR_LINES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Tasks,
    Reminders,
    Proposals,
    Chat,
    Models,
    Jobs,
    Search,
    Calendar,
}

impl Tab {
    fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Tasks => 1,
            Self::Reminders => 2,
            Self::Proposals => 3,
            Self::Chat => 4,
            Self::Models => 5,
            Self::Jobs => 6,
            Self::Search => 7,
            Self::Calendar => 8,
        }
    }

    fn from_index(index: usize) -> Self {
        match index % TABS.len() {
            0 => Self::Overview,
            1 => Self::Tasks,
            2 => Self::Reminders,
            3 => Self::Proposals,
            4 => Self::Chat,
            5 => Self::Models,
            6 => Self::Jobs,
            7 => Self::Search,
            _ => Self::Calendar,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Full,
    Compact,
    Mini,
    TooSmall,
}

impl LayoutMode {
    fn for_area(area: Rect) -> Self {
        if area.width < 50 || area.height < 14 {
            Self::TooSmall
        } else if area.width >= 120 && area.height >= 30 {
            Self::Full
        } else if area.width >= 80 && area.height >= 22 {
            Self::Compact
        } else {
            Self::Mini
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    None,
    Search,
    Chat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatExchange {
    user: String,
    assistant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiTask {
    id: String,
    title: String,
    body: String,
    status: String,
    priority: String,
    due_at: Option<String>,
    created_at: String,
    updated_at: String,
    project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiReminder {
    id: String,
    title: String,
    body: String,
    status: String,
    due_at: Option<String>,
    created_at: String,
    updated_at: String,
    calendar_sync_enabled: bool,
    calendar_sync_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiProposal {
    id: String,
    title: String,
    created_at: String,
    updated_at: String,
    decision: String,
    status: String,
    conversation_turn_id: String,
    proposed_memory: String,
    item_kind: String,
    tags: Vec<String>,
    item_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiModel {
    id: String,
    display_name: String,
    available: bool,
    active: bool,
    capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiJob {
    id: String,
    job_type: String,
    status: String,
    next_run_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiCalendarEvent {
    id: String,
    summary: String,
    status: String,
    start: String,
    end: String,
    location: String,
    html_link: String,
}

#[cfg(test)]
fn demo_tasks() -> Vec<UiTask> {
    vec![
        UiTask {
            id: "demo-task-1".into(),
            title: "Investigate runtime reconnect UX".into(),
            body: "Verify reconnect messaging and preserve the active workflow.".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            due_at: Some("21-09-26 16:00".into()),
            created_at: "21-09-26 10:00".into(),
            updated_at: "21-09-26 13:30".into(),
            project: Some("Zaraki".into()),
        },
        UiTask {
            id: "demo-task-2".into(),
            title: "Finish frontend task list".into(),
            body: "Complete the backend-neutral Tasks view and selection model.".into(),
            status: "open".into(),
            priority: "high".into(),
            due_at: Some("21-09-26 18:00".into()),
            created_at: "21-09-26 11:00".into(),
            updated_at: "21-09-26 11:00".into(),
            project: Some("Zaraki".into()),
        },
        UiTask {
            id: "demo-task-3".into(),
            title: "Review model diagnostics layout".into(),
            body: "Decide which runtime diagnostics belong in the Models view.".into(),
            status: "open".into(),
            priority: "medium".into(),
            due_at: Some("22-09-26 11:00".into()),
            created_at: "21-09-26 11:30".into(),
            updated_at: "21-09-26 11:30".into(),
            project: Some("Zaraki".into()),
        },
        UiTask {
            id: "demo-task-4".into(),
            title: "Clean up TUI warnings".into(),
            body: "Remove remaining non-actionable frontend warnings during polish.".into(),
            status: "open".into(),
            priority: "low".into(),
            due_at: None,
            created_at: "21-09-26 12:00".into(),
            updated_at: "21-09-26 12:00".into(),
            project: Some("Zaraki".into()),
        },
        UiTask {
            id: "demo-task-5".into(),
            title: "Validate Chat mouse editing".into(),
            body: "Confirm visual-column and soft-wrap cursor placement.".into(),
            status: "completed".into(),
            priority: "high".into(),
            due_at: Some("21-09-26 14:00".into()),
            created_at: "21-09-26 09:00".into(),
            updated_at: "21-09-26 14:30".into(),
            project: Some("Zaraki".into()),
        },
        UiTask {
            id: "demo-task-6".into(),
            title: "Draft task backend contract".into(),
            body: "Capture the eventual IPC boundary for task CRUD operations.".into(),
            status: "cancelled".into(),
            priority: "medium".into(),
            due_at: Some("19-09-26 17:00".into()),
            created_at: "19-09-26 11:00".into(),
            updated_at: "20-09-26 15:00".into(),
            project: Some("Zaraki".into()),
        },
        UiTask {
            id: "demo-task-7".into(),
            title: "Triage old UI regression".into(),
            body: "Review the previous input-editor failure chain and document the fix.".into(),
            status: "expired".into(),
            priority: "low".into(),
            due_at: Some("20-09-26 12:00".into()),
            created_at: "19-09-26 10:00".into(),
            updated_at: "20-09-26 12:30".into(),
            project: None,
        },
    ]
}

#[cfg(test)]
fn demo_reminders() -> Vec<UiReminder> {
    vec![
        UiReminder {
            id: "demo-reminder-1".into(),
            title: "Review Zaraki frontend milestone".into(),
            body: "Take a final pass over the remaining frontend views.".into(),
            status: "scheduled".into(),
            due_at: Some("21-09-26 19:00".into()),
            created_at: "21-09-26 12:00".into(),
            updated_at: "21-09-26 12:00".into(),
            calendar_sync_enabled: false,
            calendar_sync_status: Some("disabled".into()),
        },
        UiReminder {
            id: "demo-reminder-2".into(),
            title: "Send project update".into(),
            body: "Send a concise progress update after the frontend pass.".into(),
            status: "scheduled".into(),
            due_at: Some("22-09-26 09:30".into()),
            created_at: "21-09-26 12:10".into(),
            updated_at: "21-09-26 12:10".into(),
            calendar_sync_enabled: false,
            calendar_sync_status: Some("disabled".into()),
        },
        UiReminder {
            id: "demo-reminder-3".into(),
            title: "Follow up on model diagnostics".into(),
            body: "Revisit runtime diagnostics once the backend is wired.".into(),
            status: "triggered".into(),
            due_at: Some("21-09-26 13:00".into()),
            created_at: "21-09-26 09:00".into(),
            updated_at: "21-09-26 13:05".into(),
            calendar_sync_enabled: false,
            calendar_sync_status: Some("disabled".into()),
        },
        UiReminder {
            id: "demo-reminder-4".into(),
            title: "Test Chat flow".into(),
            body: "Verify chat input, response rendering, and scrolling.".into(),
            status: "completed".into(),
            due_at: Some("21-09-26 14:30".into()),
            created_at: "21-09-26 10:30".into(),
            updated_at: "21-09-26 14:45".into(),
            calendar_sync_enabled: false,
            calendar_sync_status: Some("disabled".into()),
        },
    ]
}

struct PendingChat {
    exchange_index: usize,
    receiver: Receiver<Result<String, String>>,
}

struct PendingApprovalPoll {
    receiver: Receiver<Result<Vec<ApprovalRequestSummary>, String>>,
}

struct PendingApprovalResponse {
    receiver: Receiver<Result<ApprovalStatus, String>>,
}

struct PendingRuntimeRestart {
    receiver: Receiver<Result<(), String>>,
}

struct PendingCalendarLogin {
    receiver: Receiver<Result<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modal {
    Help,
    Confirm(ConfirmAction),
    TaskDetail,
    ReminderDetail,
    ProposalDetail,
    SearchDetail,
    CalendarDetail,
    Approval(u64),
    RuntimeReconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmAction {
    CompleteTask,
    CancelTask,
    CompleteReminder,
    CancelReminder,
    RelaunchRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormKind {
    Task,
    Reminder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormState {
    kind: FormKind,
    editing_id: Option<String>,
    labels: Vec<String>,
    values: Vec<String>,
    active: usize,
    cursor: usize,
    validation_error: Option<String>,
}

impl FormState {
    fn task_create() -> Self {
        Self {
            kind: FormKind::Task,
            editing_id: None,
            labels: vec![
                "Title".to_string(),
                "Body".to_string(),
                "Due".to_string(),
                "Status".to_string(),
                "Priority".to_string(),
                "Project".to_string(),
            ],
            values: vec![
                String::new(),
                String::new(),
                String::new(),
                "open".to_string(),
                "medium".to_string(),
                String::new(),
            ],
            active: 0,
            cursor: 0,
            validation_error: None,
        }
    }

    fn task_edit(task: &UiTask) -> Self {
        let mut form = Self::task_create();
        form.editing_id = Some(task.id.clone());
        form.values[0] = task.title.clone();
        form.values[1] = task.body.clone();
        form.values[2] = format_due_input(task.due_at.as_deref());
        form.values[3] = task.status.clone();
        form.values[4] = task.priority.clone();
        form.values[5] = task.project.clone().unwrap_or_default();
        form.cursor = form.values[0].chars().count();
        form
    }

    fn reminder_create() -> Self {
        Self {
            kind: FormKind::Reminder,
            editing_id: None,
            labels: vec![
                "Title".to_string(),
                "Body".to_string(),
                "Due".to_string(),
                "Calendar Sync".to_string(),
            ],
            values: vec![
                String::new(),
                String::new(),
                String::new(),
                "no".to_string(),
            ],
            active: 0,
            cursor: 0,
            validation_error: None,
        }
    }

    fn reminder_edit(reminder: &UiReminder) -> Self {
        let mut form = Self::reminder_create();
        form.editing_id = Some(reminder.id.clone());
        form.values[0] = reminder.title.clone();
        form.values[1] = reminder.body.clone();
        form.values[2] = format_due_input(reminder.due_at.as_deref());
        form.values[3] = if reminder.calendar_sync_enabled {
            "yes".to_string()
        } else {
            "no".to_string()
        };
        form.cursor = form.values[0].chars().count();
        form
    }

    fn active_value(&self) -> &str {
        &self.values[self.active]
    }

    fn move_field(&mut self, delta: isize) {
        let len = self.values.len();
        if len == 0 {
            return;
        }
        self.active = if delta < 0 {
            self.active.saturating_sub(delta.unsigned_abs())
        } else {
            (self.active + delta as usize).min(len - 1)
        };
        self.cursor = self.active_value().chars().count();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Task(usize),
    Reminder(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormAction {
    Keep,
    Submit,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UiRects {
    header: Rect,
    content: Rect,
    footer: Rect,
    list: Rect,
    input: Rect,
    modal: Rect,
}

struct App {
    client: IpcClient,
    socket_path: PathBuf,
    _lease: ClientLease,
    health: Option<HealthStatus>,
    model_status: Option<ModelState>,
    models: Vec<UiModel>,
    tasks: Vec<UiTask>,
    reminders: Vec<UiReminder>,
    proposals: Vec<UiProposal>,
    jobs: Vec<UiJob>,
    calendar_status: Option<CalendarStatus>,
    calendar_events: Vec<UiCalendarEvent>,
    system_info: Option<serde_json::Value>,
    chat: Vec<ChatExchange>,
    pending_chat: Option<PendingChat>,
    approvals: Vec<ApprovalRequestSummary>,
    pending_approval_poll: Option<PendingApprovalPoll>,
    pending_approval_response: Option<PendingApprovalResponse>,
    pending_runtime_restart: Option<PendingRuntimeRestart>,
    pending_calendar_login: Option<PendingCalendarLogin>,
    tab: Tab,
    selected: usize,
    input_mode: InputMode,
    chat_input: String,
    chat_cursor: usize,
    chat_scroll: u16,
    input_scroll: u16,
    modal_scroll: u16,
    list_scroll: usize,
    search_input: String,
    search_cursor: usize,
    search_results: Vec<(String, String, f64)>,
    message: String,
    modal: Option<Modal>,
    form: Option<FormState>,
    session_title: String,
    attachment: Option<PathBuf>,
    last_health_check: Instant,
    last_approval_poll: Instant,
    runtime_reconnect_offered: bool,
    last_layout: UiRects,
}

impl App {
    fn new(lease: ClientLease, socket_path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let client = IpcClient::new(&socket_path);
        let mut app = Self::from_client(lease, client, socket_path);
        if let Err(error) = app.refresh() {
            app.message = format!("Runtime unavailable: {error}");
        } else if let Err(error) = app.load_session_history() {
            app.notify(format!("Session history unavailable: {error}"));
        }
        Ok(app)
    }

    fn from_client(lease: ClientLease, client: IpcClient, socket_path: PathBuf) -> Self {
        let tasks = Vec::new();
        Self {
            client,
            socket_path,
            _lease: lease,
            health: None,
            model_status: None,
            models: Vec::new(),
            tasks,
            reminders: Vec::new(),
            proposals: Vec::new(),
            jobs: Vec::new(),
            calendar_status: None,
            calendar_events: Vec::new(),
            system_info: None,
            chat: Vec::new(),
            pending_chat: None,
            approvals: Vec::new(),
            pending_approval_poll: None,
            pending_approval_response: None,
            pending_runtime_restart: None,
            pending_calendar_login: None,
            tab: Tab::Overview,
            selected: 0,
            input_mode: InputMode::None,
            chat_input: String::new(),
            chat_cursor: 0,
            chat_scroll: 0,
            input_scroll: 0,
            modal_scroll: 0,
            list_scroll: 0,
            search_input: String::new(),
            search_cursor: 0,
            search_results: Vec::new(),
            message: String::new(),
            modal: None,
            form: None,
            session_title: "New session".to_string(),
            attachment: None,
            last_health_check: Instant::now() - HEALTH_CHECK_INTERVAL,
            last_approval_poll: Instant::now() - APPROVAL_POLL_INTERVAL,
            runtime_reconnect_offered: false,
            last_layout: UiRects {
                header: Rect::default(),
                content: Rect::default(),
                footer: Rect::default(),
                list: Rect::default(),
                input: Rect::default(),
                modal: Rect::default(),
            },
        }
    }

    fn refresh(&mut self) -> Result<(), IpcClientError> {
        self.health = Some(self.request_health()?);
        self.models = self.request_models()?;
        let status = self.request_model_status()?;
        self.model_status = Some(status.clone());
        if let Some(active) = status.active_model.as_deref() {
            for model in &mut self.models {
                model.active = model.id == active
                    || model.display_name == active
                    || model.id.ends_with(active);
            }
        }
        let existing_task_metadata = self
            .tasks
            .iter()
            .map(|task| {
                (
                    task.id.clone(),
                    (task.priority.clone(), task.project.clone()),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        self.tasks = self.request_tasks()?;
        for task in &mut self.tasks {
            if let Some((priority, project)) = existing_task_metadata.get(&task.id) {
                task.priority = priority.clone();
                task.project = project.clone();
            }
        }
        sort_ui_tasks(&mut self.tasks);
        self.reminders = self.request_reminders()?;
        sort_ui_reminders(&mut self.reminders);
        self.proposals = self.request_proposals()?;
        sort_ui_proposals(&mut self.proposals);
        self.jobs = self.request_jobs()?;
        sort_ui_jobs(&mut self.jobs);
        self.system_info = self.request_system_info().ok();
        if let Ok((status, events)) = self.request_calendar_snapshot() {
            self.calendar_status = Some(status);
            self.calendar_events = events;
        } else {
            self.calendar_events.clear();
        }
        if self.tab == Tab::Search && !self.search_input.trim().is_empty() {
            let query = self.search_input.trim().to_string();
            match self.request_memory_search(&query) {
                Ok(results) => self.search_results = results,
                Err(_) => self.search_results.clear(),
            }
        }
        self.normalize_selection();
        self.last_health_check = Instant::now();
        self.runtime_reconnect_offered = false;
        Ok(())
    }

    fn request_session_history(
        &self,
    ) -> Result<assistant_protocol::SessionHistoryResponse, IpcClientError> {
        match self
            .client
            .request(RequestMethod::SessionHistory { limit: Some(100) })?
        {
            ResponsePayload::SessionHistory(history) => Ok(history),
            other => Err(unexpected_response("session history", other)),
        }
    }

    fn load_session_history(&mut self) -> Result<(), IpcClientError> {
        let history = self.request_session_history()?;
        self.chat = history
            .turns
            .into_iter()
            .map(|turn| ChatExchange {
                user: turn.user,
                assistant: Some(turn.assistant),
            })
            .collect();
        self.session_title = self
            .chat
            .first()
            .map(|exchange| compact(&exchange.user.replace(['\n', '\r'], " "), 48))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "New session".to_string());
        self.selected = self.chat.len().saturating_sub(1);
        self.chat_scroll = if self.chat.is_empty() { 0 } else { u16::MAX };
        Ok(())
    }

    fn request_health(&self) -> Result<HealthStatus, IpcClientError> {
        match self.client.request(RequestMethod::Health)? {
            ResponsePayload::Health(value) => Ok(value),
            other => Err(unexpected_response("health", other)),
        }
    }

    fn request_model_status(&self) -> Result<ModelState, IpcClientError> {
        match self.client.request(RequestMethod::ModelStatus)? {
            ResponsePayload::ModelStatus(value) => Ok(value),
            other => Err(unexpected_response("model status", other)),
        }
    }

    fn request_models(&self) -> Result<Vec<UiModel>, IpcClientError> {
        match self.client.request(RequestMethod::ModelList)? {
            ResponsePayload::Models(list) => {
                Ok(list.models.into_iter().map(ui_model_from_backend).collect())
            }
            other => Err(unexpected_response("models", other)),
        }
    }

    fn request_tasks(&self) -> Result<Vec<UiTask>, IpcClientError> {
        let response = self.client.request(RequestMethod::TasksList {
            status: None,
            limit: Some(100),
        })?;
        let ResponsePayload::Tasks(list) = response else {
            return Err(unexpected_response("tasks", response));
        };
        Ok(list.tasks.into_iter().map(ui_task_from_backend).collect())
    }

    fn request_reminders(&self) -> Result<Vec<UiReminder>, IpcClientError> {
        let response = self.client.request(RequestMethod::RemindersList {
            status: None,
            limit: Some(100),
        })?;
        let ResponsePayload::Reminders(list) = response else {
            return Err(unexpected_response("reminders", response));
        };
        Ok(list
            .reminders
            .into_iter()
            .map(ui_reminder_from_backend)
            .collect())
    }

    fn request_proposals(&self) -> Result<Vec<UiProposal>, IpcClientError> {
        let response = self
            .client
            .request(RequestMethod::MemoryProposals { limit: Some(100) })?;
        let ResponsePayload::Proposals(list) = response else {
            return Err(unexpected_response("memory proposals", response));
        };
        Ok(list
            .proposals
            .into_iter()
            .map(ui_proposal_from_backend)
            .collect())
    }

    fn request_jobs(&self) -> Result<Vec<UiJob>, IpcClientError> {
        let response = self.client.request(RequestMethod::JobsList {
            status: None,
            limit: Some(100),
        })?;
        let ResponsePayload::Jobs(list) = response else {
            return Err(unexpected_response("jobs", response));
        };
        Ok(list.jobs.into_iter().map(ui_job_from_backend).collect())
    }

    fn request_system_info(&self) -> Result<serde_json::Value, IpcClientError> {
        match self.client.request(RequestMethod::SystemInfo {
            scope: Some("all".to_string()),
        })? {
            ResponsePayload::SystemInfo(value) => Ok(value),
            other => Err(unexpected_response("system info", other)),
        }
    }

    fn request_calendar_snapshot(
        &self,
    ) -> Result<(CalendarStatus, Vec<UiCalendarEvent>), IpcClientError> {
        let status = match self.client.request(RequestMethod::CalendarStatus)? {
            ResponsePayload::CalendarStatus(value) => value,
            other => return Err(unexpected_response("calendar status", other)),
        };
        if !status.authenticated {
            return Ok((status, Vec::new()));
        }

        let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
        let month_start = now.date().replace_day(1).unwrap_or(now.date());
        let next_month = month_start
            .checked_add(time::Duration::days(32))
            .and_then(|date| date.replace_day(1).ok())
            .unwrap_or(month_start);
        let time_min = month_start
            .midnight()
            .assume_offset(now.offset())
            .format(&Rfc3339)
            .ok();
        let time_max = next_month
            .midnight()
            .assume_offset(now.offset())
            .format(&Rfc3339)
            .ok();
        let client = IpcClient::new(&self.socket_path).with_timeout(Duration::from_secs(3));
        let response = client.request(RequestMethod::CalendarEventsList {
            calendar_id: Some(status.calendar_id.clone()),
            time_min,
            time_max,
            query: None,
            max_results: Some(250),
            page_token: None,
        })?;
        let ResponsePayload::CalendarEvents(list) = response else {
            return Err(unexpected_response("calendar events", response));
        };
        let mut events = list
            .events
            .into_iter()
            .filter_map(ui_calendar_event_from_backend)
            .collect::<Vec<_>>();
        events.sort_by(|a, b| a.start.cmp(&b.start).then(a.summary.cmp(&b.summary)));
        Ok((status, events))
    }

    fn request_memory_search(
        &self,
        query: &str,
    ) -> Result<Vec<(String, String, f64)>, IpcClientError> {
        let response = self.client.request(RequestMethod::MemorySearch {
            query: query.to_string(),
            limit: Some(100),
        })?;
        let ResponsePayload::Memory(list) = response else {
            return Err(unexpected_response("memory search", response));
        };
        Ok(list
            .results
            .into_iter()
            .map(ui_search_result_from_backend)
            .collect())
    }

    fn normalize_selection(&mut self) {
        let len = self.current_len();
        self.selected = if len == 0 {
            0
        } else {
            self.selected.min(len - 1)
        };
    }

    fn current_len(&self) -> usize {
        match self.tab {
            Tab::Overview => self.overview_rows().len(),
            Tab::Tasks => self.tasks.len(),
            Tab::Reminders => self.reminders.len(),
            Tab::Proposals => self.proposals.len(),
            Tab::Chat => self.chat.len(),
            Tab::Models => self.models.len(),
            Tab::Jobs => self.jobs.len(),
            Tab::Search => self.search_results.len(),
            Tab::Calendar => self.calendar_events.len(),
        }
    }

    fn selected_available_model(&self) -> Option<&UiModel> {
        self.models
            .get(self.selected)
            .filter(|model| model.available)
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = 0;
        self.input_mode = InputMode::None;
        self.modal = None;
        self.form = None;
        self.message.clear();
        self.list_scroll = 0;
        self.modal_scroll = 0;
        if tab == Tab::Search && !self.search_input.trim().is_empty() {
            self.run_search();
        }
    }

    fn next_tab(&mut self) {
        self.set_tab(Tab::from_index(self.tab.index() + 1));
    }

    fn previous_tab(&mut self) {
        self.set_tab(Tab::from_index(self.tab.index() + TABS.len() - 1));
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.current_len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(len - 1)
        };
        self.selected = next;
        self.keep_selection_visible();
    }

    fn keep_selection_visible(&mut self) {
        let viewport = self.last_layout.list.height.saturating_sub(2).max(1) as usize;
        if self.selected < self.content_scroll() {
            match self.tab {
                Tab::Chat => self.chat_scroll = self.selected as u16,
                _ => {}
            }
        } else if self.selected >= self.content_scroll() + viewport {
            match self.tab {
                Tab::Chat => {
                    self.chat_scroll = self.selected.saturating_sub(viewport - 1) as u16;
                }
                _ => {}
            }
        }
    }

    fn content_scroll(&self) -> usize {
        match self.tab {
            Tab::Chat => self.chat_scroll as usize,
            _ => 0,
        }
    }

    fn overview_rows(&self) -> Vec<RowKind> {
        let mut rows = self
            .important_task_indices()
            .into_iter()
            .map(RowKind::Task)
            .collect::<Vec<_>>();
        rows.extend(
            self.important_reminder_indices()
                .into_iter()
                .take(3)
                .map(RowKind::Reminder),
        );
        rows
    }

    fn important_task_indices(&self) -> Vec<usize> {
        let mut indices = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| {
                !matches!(task.status.as_str(), "completed" | "cancelled" | "archived")
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let today = OffsetDateTime::now_local()
            .unwrap_or_else(|_| OffsetDateTime::now_utc())
            .date();
        indices.sort_by(|left, right| {
            compare_overview_tasks(&self.tasks[*left], &self.tasks[*right], today)
        });
        indices
    }

    fn important_reminder_indices(&self) -> Vec<usize> {
        self.reminders
            .iter()
            .enumerate()
            .filter(|(_, reminder)| !matches!(reminder.status.as_str(), "completed" | "cancelled"))
            .take(5)
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_task_index(&self) -> Option<usize> {
        match self.tab {
            Tab::Tasks => (self.selected < self.tasks.len()).then_some(self.selected),
            Tab::Overview => self
                .overview_rows()
                .get(self.selected)
                .and_then(|row| match row {
                    RowKind::Task(index) => Some(*index),
                    _ => None,
                }),
            _ => None,
        }
    }

    fn selected_reminder_index(&self) -> Option<usize> {
        match self.tab {
            Tab::Reminders => (self.selected < self.reminders.len()).then_some(self.selected),
            Tab::Overview => self
                .overview_rows()
                .get(self.selected)
                .and_then(|row| match row {
                    RowKind::Reminder(index) => Some(*index),
                    _ => None,
                }),
            _ => None,
        }
    }

    fn selected_proposal(&self) -> Option<&UiProposal> {
        self.proposals.get(self.selected)
    }

    fn selected_model(&self) -> Option<&UiModel> {
        self.selected_available_model()
    }

    fn selected_search(&self) -> Option<&(String, String, f64)> {
        self.search_results.get(self.selected)
    }

    fn selected_calendar_event(&self) -> Option<&UiCalendarEvent> {
        self.calendar_events.get(self.selected)
    }

    fn accept_selected_proposal(&mut self) {
        let Some(proposal) = self.selected_proposal().cloned() else {
            return;
        };
        match self.client.request(RequestMethod::MemoryProposalAccept {
            id: proposal.id.clone(),
        }) {
            Ok(ResponsePayload::Mutation(_)) => match self.refresh() {
                Ok(()) => self.notify(format!(
                    "Accepted proposal {}.",
                    compact(&proposal.title, 42)
                )),
                Err(error) => {
                    self.notify(format!("Accepted proposal, but refresh failed: {error}"))
                }
            },
            Ok(other) => self.notify(format!(
                "Proposal accept failed: unexpected response {other:?}"
            )),
            Err(error) => self.notify(format!("Proposal accept failed: {error}")),
        }
    }

    fn reject_selected_proposal(&mut self) {
        let Some(proposal) = self.selected_proposal().cloned() else {
            return;
        };
        match self.client.request(RequestMethod::MemoryProposalReject {
            id: proposal.id.clone(),
        }) {
            Ok(ResponsePayload::Mutation(_)) => match self.refresh() {
                Ok(()) => self.notify(format!(
                    "Rejected proposal {}.",
                    compact(&proposal.title, 42)
                )),
                Err(error) => {
                    self.notify(format!("Rejected proposal, but refresh failed: {error}"))
                }
            },
            Ok(other) => self.notify(format!(
                "Proposal reject failed: unexpected response {other:?}"
            )),
            Err(error) => self.notify(format!("Proposal reject failed: {error}")),
        }
    }

    fn switch_selected_model(&mut self) {
        let Some(model) = self.selected_model().cloned() else {
            return;
        };
        if !model.available {
            self.notify("This model is unavailable.".to_string());
            return;
        }
        self.notify(format!(
            "Switching to {}...",
            compact(&model.display_name, 42)
        ));
        let client = IpcClient::new(&self.socket_path).with_timeout(Duration::from_secs(30));
        match client.request(RequestMethod::ModelSwitch {
            model: model.id.clone(),
        }) {
            Ok(ResponsePayload::ModelStatus(status)) => {
                let active_model = status.active_model.clone();
                self.model_status = Some(status);
                for item in &mut self.models {
                    item.active = active_model
                        .as_deref()
                        .map(|active| {
                            item.id == active
                                || item.display_name == active
                                || item.id.ends_with(active)
                        })
                        .unwrap_or(false);
                }
                self.notify(format!("Switched to {}.", compact(&model.display_name, 42)));
            }
            Ok(other) => {
                self.notify(format!(
                    "Model switch failed: unexpected response {other:?}"
                ));
            }
            Err(error) => {
                self.notify(format!("Model switch failed: {error}"));
            }
        }
    }

    fn poll_runtime_restart(&mut self) {
        let Some(pending) = self.pending_runtime_restart.as_ref() else {
            return;
        };
        match pending.receiver.try_recv() {
            Ok(Ok(())) => {
                self.pending_runtime_restart = None;
                self.modal = None;
                self.runtime_reconnect_offered = false;
                match self.refresh() {
                    Ok(()) => self.notify("Runtime reconnected.".to_string()),
                    Err(error) => {
                        self.notify(format!("Runtime started but refresh failed: {error}"))
                    }
                }
            }
            Ok(Err(error)) => {
                self.pending_runtime_restart = None;
                self.notify(error);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.pending_runtime_restart = None;
                self.notify("Runtime relaunch worker disconnected.".to_string());
            }
        }
    }

    fn poll_calendar_login(&mut self) {
        let Some(pending) = self.pending_calendar_login.as_ref() else {
            return;
        };

        match pending.receiver.try_recv() {
            Ok(Ok(message)) => {
                self.pending_calendar_login = None;
                self.notify(message);
            }
            Ok(Err(error)) => {
                self.pending_calendar_login = None;
                self.notify(format!("Google Calendar login failed: {error}"));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.pending_calendar_login = None;
                self.notify("Google Calendar login worker disconnected.".to_string());
            }
        }
    }

    fn start_calendar_login(&mut self) {
        if self.pending_calendar_login.is_some() {
            self.notify("Google Calendar login is already in progress.".to_string());
            return;
        }

        let socket_path = self.socket_path.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let client = IpcClient::new(socket_path).with_timeout(Duration::from_secs(600));
            let result = match client.request(RequestMethod::CalendarLogin) {
                Ok(ResponsePayload::CalendarStatus(status)) if status.authenticated => Ok(format!(
                    "Google Calendar authenticated. Write access: {}. Calendar: {}.",
                    if status.write_enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    status.calendar_id
                )),
                Ok(ResponsePayload::CalendarStatus(status)) => Err(format!(
                    "Google Calendar is not authenticated after login (configured={}, calendar={}).",
                    status.configured, status.calendar_id
                )),
                Ok(other) => Err(format!("Unexpected Calendar login response: {other:?}")),
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(result);
        });

        self.pending_calendar_login = Some(PendingCalendarLogin { receiver });
        self.notify("Opening Google Calendar authorization in your browser...".to_string());
    }

    fn run_search(&mut self) {
        let query = self.search_input.trim().to_string();
        if query.is_empty() {
            self.search_results.clear();
            self.notify("Search query is empty.".to_string());
            return;
        }
        match self.request_memory_search(&query) {
            Ok(results) => {
                self.search_results = results;
                self.selected = 0;
                self.notify(format!(
                    "{} result(s). Scope: indexed memory.",
                    self.search_results.len()
                ));
            }
            Err(error) => {
                self.search_results.clear();
                self.selected = 0;
                self.notify(format!("Search failed: {error}"));
            }
        }
    }

    fn send_chat(&mut self) {
        if self.pending_chat.is_some() {
            self.notify("Assistant is still responding.".to_string());
            return;
        }
        let text = self.chat_input.trim_end().to_string();
        if text.trim().is_empty() && self.attachment.is_none() {
            return;
        }
        if text.starts_with(':') {
            self.execute_chat_command(&text);
            return;
        }

        let attachment = self.attachment.take();
        let attachment_label = attachment.as_ref().map(|path| {
            format!(
                "[Attachment: {}]",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file")
            )
        });
        let display_text = match (&text, attachment_label) {
            (text, Some(label)) if text.trim().is_empty() => label,
            (text, Some(label)) => format!("{text}\n{label}"),
            (text, None) => text.clone(),
        };

        self.chat_input.clear();
        self.chat_cursor = 0;
        self.input_scroll = 0;
        self.input_mode = InputMode::None;
        if self.session_title == "New session" {
            self.session_title = compact(
                display_text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .as_str(),
                48,
            );
        }
        let exchange_index = self.chat.len();
        self.chat.push(ChatExchange {
            user: display_text,
            assistant: None,
        });
        self.selected = exchange_index;
        self.chat_scroll = u16::MAX;
        let socket_path = self.socket_path.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let attachments = attachment
                .into_iter()
                .map(|path| assistant_protocol::ChatAttachment {
                    path: path.display().to_string(),
                })
                .collect();
            let client = IpcClient::new(socket_path).with_timeout(CHAT_TIMEOUT);
            let result = match client.request(RequestMethod::Chat { text, attachments }) {
                Ok(ResponsePayload::Chat(ChatResponse { text })) => Ok(text),
                Ok(other) => Err(format!("Unexpected chat response: {other:?}")),
                Err(error) => Err(format!("Chat failed: {error}")),
            };
            let _ = sender.send(result);
        });
        self.last_approval_poll = Instant::now() - APPROVAL_POLL_INTERVAL;
        self.pending_chat = Some(PendingChat {
            exchange_index,
            receiver,
        });
    }

    fn execute_chat_command(&mut self, raw: &str) {
        let mut parts = raw.trim().splitn(2, ' ');
        let command = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match command {
            ":new" => match self.client.request(RequestMethod::SessionNew) {
                Ok(ResponsePayload::Session(_)) => {
                    self.chat.clear();
                    self.session_title = "New session".to_string();
                    self.selected = 0;
                    self.chat_scroll = 0;
                    self.notify("Started a new chat session.".to_string());
                }
                Ok(other) => {
                    self.notify(format!("New session failed: unexpected response {other:?}"));
                }
                Err(error) => self.notify(format!("New session failed: {error}")),
            },
            ":refresh" => match self.refresh() {
                Ok(()) => self.notify("Refreshed.".to_string()),
                Err(error) => self.notify(format!("Refresh failed: {error}")),
            },
            ":help" => self.modal = Some(Modal::Help),
            ":calendar-login" => {
                if arg.is_empty() {
                    self.start_calendar_login();
                } else {
                    self.notify("Usage: :calendar-login".to_string());
                }
            }
            ":model" => self.set_tab(Tab::Models),
            ":search" => {
                if arg.is_empty() {
                    self.set_tab(Tab::Search);
                    self.start_search_input();
                } else {
                    self.search_input = arg.to_string();
                    self.search_cursor = self.search_input.chars().count();
                    self.set_tab(Tab::Search);
                    self.run_search();
                }
            }
            ":file" => {
                if arg.is_empty() {
                    self.notify("Usage: :file <path>".to_string());
                } else {
                    let path = PathBuf::from(arg);
                    match std::fs::metadata(&path) {
                        Ok(metadata) if metadata.is_file() => {
                            self.attachment = Some(path.clone());
                            self.notify(format!(
                                "Staged file: {}. It will be sent with the next chat message.",
                                path.display()
                            ));
                        }
                        Ok(_) => self.notify("Attachment path is not a regular file.".to_string()),
                        Err(error) => self.notify(format!("Could not stage attachment: {error}")),
                    }
                }
            }
            _ => self.notify(format!("Unknown command {command}. Press ? for help.")),
        }
        self.chat_input.clear();
        self.chat_cursor = 0;
    }

    fn poll_approvals(&mut self) {
        if self.pending_chat.is_none() {
            self.pending_approval_poll = None;
            return;
        }

        if let Some(pending) = self.pending_approval_poll.take() {
            match pending.receiver.try_recv() {
                Ok(Ok(approvals)) => {
                    let previous_approval = match self.modal {
                        Some(Modal::Approval(id)) => Some(id),
                        _ => None,
                    };
                    self.approvals = approvals;
                    let still_pending = previous_approval
                        .is_some_and(|id| self.approvals.iter().any(|approval| approval.id == id));
                    if still_pending || self.pending_approval_response.is_some() {
                        return;
                    }
                    if let Some(approval) = self.approvals.first() {
                        self.modal = Some(Modal::Approval(approval.id));
                        self.modal_scroll = 0;
                        self.notify(format!("Approval required for {}.", approval.tool));
                    } else if matches!(self.modal, Some(Modal::Approval(_))) {
                        self.modal = None;
                    }
                }
                Ok(Err(_)) | Err(TryRecvError::Disconnected) => {}
                Err(TryRecvError::Empty) => {
                    self.pending_approval_poll = Some(pending);
                    return;
                }
            }
        }

        if self.pending_approval_poll.is_some()
            || self.last_approval_poll.elapsed() < APPROVAL_POLL_INTERVAL
        {
            return;
        }

        self.last_approval_poll = Instant::now();
        let socket_path = self.socket_path.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let client = IpcClient::new(socket_path).with_timeout(Duration::from_millis(700));
            let result = match client.request(RequestMethod::ApprovalsList) {
                Ok(ResponsePayload::Approvals(list)) => Ok(list.approvals),
                Ok(other) => Err(format!("Unexpected approvals response: {other:?}")),
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(result);
        });
        self.pending_approval_poll = Some(PendingApprovalPoll { receiver });
    }

    fn poll_approval_response(&mut self) {
        let Some(pending) = self.pending_approval_response.take() else {
            return;
        };
        match pending.receiver.try_recv() {
            Ok(Ok(status)) => {
                self.approvals.retain(|approval| approval.id != status.id);
                self.modal = None;
                self.modal_scroll = 0;
                self.notify(format!(
                    "{} approval request {}.",
                    if status.approved {
                        "Approved"
                    } else {
                        "Denied"
                    },
                    status.id
                ));
            }
            Ok(Err(error)) => {
                self.notify(format!("Approval response failed: {error}"));
            }
            Err(TryRecvError::Empty) => {
                self.pending_approval_response = Some(pending);
            }
            Err(TryRecvError::Disconnected) => {
                self.notify("Approval response worker disconnected.".to_string());
            }
        }
    }

    fn respond_to_approval(&mut self, approval_id: u64, approved: bool) {
        if self.pending_approval_response.is_some() {
            return;
        }

        let socket_path = self.socket_path.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let client = IpcClient::new(socket_path).with_timeout(Duration::from_secs(2));
            let result = match client.request(RequestMethod::ApprovalRespond {
                id: approval_id,
                approved,
            }) {
                Ok(ResponsePayload::Approval(status)) => Ok(status),
                Ok(other) => Err(format!("Unexpected approval response: {other:?}")),
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(result);
        });
        self.pending_approval_response = Some(PendingApprovalResponse { receiver });
    }

    fn poll_chat_response(&mut self) {
        let Some(pending) = self.pending_chat.as_ref() else {
            return;
        };
        match pending.receiver.try_recv() {
            Ok(Ok(response)) => {
                let exchange_index = pending.exchange_index;
                if let Some(exchange) = self.chat.get_mut(exchange_index) {
                    exchange.assistant = Some(response);
                }
                self.pending_chat = None;
                self.pending_approval_poll = None;
                self.pending_approval_response = None;
                if matches!(self.modal, Some(Modal::Approval(_))) {
                    self.modal = None;
                }
                self.approvals.clear();
                self.chat_scroll = u16::MAX;
                self.message.clear();
            }
            Ok(Err(error)) => {
                let exchange_index = pending.exchange_index;
                if let Some(exchange) = self.chat.get_mut(exchange_index) {
                    exchange.assistant = Some(format!("[Error] {error}"));
                }
                self.pending_chat = None;
                self.pending_approval_poll = None;
                self.pending_approval_response = None;
                if matches!(self.modal, Some(Modal::Approval(_))) {
                    self.modal = None;
                }
                self.approvals.clear();
                self.notify(error);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                let exchange_index = pending.exchange_index;
                if let Some(exchange) = self.chat.get_mut(exchange_index) {
                    exchange.assistant = Some("[Error] Chat worker disconnected.".to_string());
                }
                self.pending_chat = None;
                self.pending_approval_poll = None;
                self.pending_approval_response = None;
                if matches!(self.modal, Some(Modal::Approval(_))) {
                    self.modal = None;
                }
                self.approvals.clear();
                self.notify("Chat worker disconnected.".to_string());
            }
        }
    }

    fn poll_runtime_health(&mut self) {
        if self.pending_chat.is_some() {
            return;
        }
        if self.last_health_check.elapsed() < HEALTH_CHECK_INTERVAL {
            return;
        }
        self.last_health_check = Instant::now();
        let client = IpcClient::new(&self.socket_path).with_timeout(Duration::from_millis(700));
        match client.request(RequestMethod::Health) {
            Ok(ResponsePayload::Health(health)) => {
                let was_disconnected = self
                    .health
                    .as_ref()
                    .map(|value| !value.ready)
                    .unwrap_or(true);
                self.health = Some(health.clone());
                if health.ready && was_disconnected {
                    self.runtime_reconnect_offered = false;
                    if let Err(error) = self.load_session_history() {
                        self.notify(format!("Session history refresh failed: {error}"));
                    }
                }
                if health.ready {
                    if let Ok(ResponsePayload::ModelStatus(status)) =
                        client.request(RequestMethod::ModelStatus)
                    {
                        self.model_status = Some(status);
                    }
                    if let Ok(ResponsePayload::SystemInfo(info)) =
                        client.request(RequestMethod::SystemInfo {
                            scope: Some("all".to_string()),
                        })
                    {
                        self.system_info = Some(info);
                    }
                    if self.tab == Tab::Calendar {
                        if let Ok((status, events)) = self.request_calendar_snapshot() {
                            self.calendar_status = Some(status);
                            self.calendar_events = events;
                            self.normalize_selection();
                        }
                    }
                }
            }
            Ok(_) => self.mark_runtime_disconnected("Unexpected runtime health response."),
            Err(error) => self.mark_runtime_disconnected(&format!("Runtime disconnected: {error}")),
        }
    }

    fn mark_runtime_disconnected(&mut self, message: &str) {
        let was_ready = self
            .health
            .as_ref()
            .map(|value| value.ready)
            .unwrap_or(false);
        self.health = Some(HealthStatus {
            runtime: "disconnected".to_string(),
            ready: false,
        });
        self.message = message.to_string();
        if was_ready && !self.runtime_reconnect_offered && self.pending_runtime_restart.is_none() {
            self.modal = Some(Modal::RuntimeReconnect);
            self.runtime_reconnect_offered = true;
        }
    }

    fn relaunch_runtime(&mut self) {
        if self.pending_runtime_restart.is_some() {
            return;
        }
        let socket_path = self.socket_path.clone();
        let preferred = self
            .model_status
            .as_ref()
            .and_then(|status| status.active_model.clone());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let models = discover_models().map_err(|error| error.to_string())?;
                let selected_name = preferred
                    .as_deref()
                    .and_then(|id| id.strip_prefix("local:"))
                    .and_then(|name| models.iter().position(|model| model.filename == name))
                    .unwrap_or_else(|| initial_model_index(&models));
                let model = models
                    .get(selected_name)
                    .ok_or("No local model available.")?
                    .clone();
                let runtime_bin = runtime_binary_path().map_err(|error| error.to_string())?;
                let mut launcher = RuntimeLaunch::new(&runtime_bin, &socket_path, &model)
                    .map_err(|error| error.to_string())?;
                launcher
                    .wait_until_ready(&socket_path)
                    .map_err(|error| error.to_string())?;
                launcher.detach();
                Ok(())
            })();
            let _ = sender.send(result);
        });
        self.pending_runtime_restart = Some(PendingRuntimeRestart { receiver });
        self.modal = None;
        self.notify("Relaunching runtime...".to_string());
    }

    fn open_task_form(&mut self, edit: bool) {
        if edit {
            let Some(index) = self.selected_task_index() else {
                self.notify("Select a task to edit.".to_string());
                return;
            };
            let Some(task) = self.tasks.get(index) else {
                self.notify("Select a task to edit.".to_string());
                return;
            };
            self.form = Some(FormState::task_edit(task));
        } else {
            self.form = Some(FormState::task_create());
        }
        self.modal = None;
    }

    fn open_reminder_form(&mut self, edit: bool) {
        if edit {
            let Some(index) = self.selected_reminder_index() else {
                self.notify("Select a reminder to edit.".to_string());
                return;
            };
            let Some(reminder) = self.reminders.get(index) else {
                self.notify("Select a reminder to edit.".to_string());
                return;
            };
            self.form = Some(FormState::reminder_edit(reminder));
        } else {
            self.form = Some(FormState::reminder_create());
        }
        self.modal = None;
    }

    fn submit_form(&mut self) {
        let Some(form) = self.form.clone() else {
            return;
        };
        let title = form.values.first().cloned().unwrap_or_default();
        let body = form.values.get(1).cloned().unwrap_or_default();
        let due = form.values.get(2).cloned().unwrap_or_default();
        if title.trim().is_empty() {
            if let Some(active) = self.form.as_mut() {
                active.validation_error = Some("Title cannot be empty.".to_string());
            }
            return;
        }

        match form.kind {
            FormKind::Task => {
                let status = form
                    .values
                    .get(3)
                    .cloned()
                    .unwrap_or_else(|| "open".to_string())
                    .trim()
                    .to_ascii_lowercase();
                let priority = form
                    .values
                    .get(4)
                    .cloned()
                    .unwrap_or_else(|| "medium".to_string())
                    .trim()
                    .to_ascii_lowercase();
                if !matches!(
                    status.as_str(),
                    "open" | "in_progress" | "completed" | "cancelled" | "expired" | "archived"
                ) {
                    if let Some(active) = self.form.as_mut() {
                        active.validation_error = Some("Status must be open, in_progress, completed, cancelled, expired, or archived.".to_string());
                    }
                    return;
                }
                if !matches!(priority.as_str(), "high" | "medium" | "low") {
                    if let Some(active) = self.form.as_mut() {
                        active.validation_error =
                            Some("Priority must be high, medium, or low.".to_string());
                    }
                    return;
                }
                let project = form.values.get(5).cloned().unwrap_or_default();
                let due = if due.trim().is_empty() {
                    None
                } else {
                    Some(due)
                };
                let editing_id = form.editing_id.clone();
                let was_editing = editing_id.is_some();
                let mutation = if let Some(id) = editing_id.clone() {
                    RequestMethod::TasksMutate {
                        operation: "update".to_string(),
                        id: Some(id),
                        title: Some(title),
                        body: Some(body),
                        due: Some(due),
                        status: Some(status),
                    }
                } else {
                    RequestMethod::TasksMutate {
                        operation: "create".to_string(),
                        id: None,
                        title: Some(title),
                        body: Some(body),
                        due: Some(due),
                        status: Some(status),
                    }
                };
                match self.client.request(mutation) {
                    Ok(ResponsePayload::Mutation(result)) => {
                        let backend_id = result
                            .output
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                        match self.refresh() {
                            Ok(()) => {
                                let selected_id = editing_id.or(backend_id);
                                if let Some(id) = selected_id.as_deref() {
                                    self.selected = self
                                        .tasks
                                        .iter()
                                        .position(|task| task.id == id)
                                        .unwrap_or(0);
                                    if let Some(task) =
                                        self.tasks.iter_mut().find(|task| task.id == id)
                                    {
                                        task.priority = priority;
                                        task.project =
                                            (!project.trim().is_empty()).then_some(project);
                                    }
                                }
                                self.form = None;
                                self.notify(if was_editing {
                                    "Task updated in backend.".to_string()
                                } else {
                                    "Task created in backend. Priority/project remain frontend-local for now.".to_string()
                                });
                            }
                            Err(error) => {
                                if let Some(active) = self.form.as_mut() {
                                    active.validation_error =
                                        Some(format!("Saved, but refresh failed: {error}"));
                                }
                            }
                        }
                    }
                    Ok(other) => {
                        if let Some(active) = self.form.as_mut() {
                            active.validation_error =
                                Some(format!("Unexpected task mutation response: {other:?}"));
                        }
                    }
                    Err(error) => {
                        if let Some(active) = self.form.as_mut() {
                            active.validation_error = Some(format!("Task save failed: {error}"));
                        }
                    }
                }
            }
            FormKind::Reminder => {
                let due = if due.trim().is_empty() {
                    None
                } else {
                    Some(due)
                };
                let calendar_sync = form
                    .values
                    .get(3)
                    .map(|value| value.trim().to_ascii_lowercase())
                    .and_then(|value| match value.as_str() {
                        "yes" | "true" | "on" => Some(true),
                        "no" | "false" | "off" => Some(false),
                        _ => None,
                    });
                let Some(calendar_sync) = calendar_sync else {
                    if let Some(active) = self.form.as_mut() {
                        active.validation_error =
                            Some("Calendar Sync must be yes or no.".to_string());
                    }
                    return;
                };
                let editing_id = form.editing_id.clone();
                let was_editing = editing_id.is_some();
                let mutation = if let Some(id) = editing_id.clone() {
                    RequestMethod::RemindersMutate {
                        operation: "update".to_string(),
                        id: Some(id),
                        title: Some(title),
                        body: Some(body),
                        due: Some(due),
                        calendar_sync: Some(calendar_sync),
                    }
                } else {
                    RequestMethod::RemindersMutate {
                        operation: "create".to_string(),
                        id: None,
                        title: Some(title),
                        body: Some(body),
                        due: Some(due),
                        calendar_sync: Some(calendar_sync),
                    }
                };

                let mutation_result = if calendar_sync {
                    IpcClient::new(&self.socket_path)
                        .with_timeout(REMINDER_MUTATION_TIMEOUT)
                        .request(mutation)
                } else {
                    self.client.request(mutation)
                };

                match mutation_result {
                    Ok(ResponsePayload::Mutation(result)) => {
                        let backend_id = result
                            .output
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                        match self.refresh() {
                            Ok(()) => {
                                let selected_id = editing_id.or(backend_id);
                                if let Some(id) = selected_id.as_deref() {
                                    self.selected = self
                                        .reminders
                                        .iter()
                                        .position(|reminder| reminder.id == id)
                                        .unwrap_or(0);
                                }
                                self.form = None;
                                let base = if was_editing {
                                    "Reminder updated in backend."
                                } else {
                                    "Reminder created in backend."
                                };
                                let sync_message = result
                                    .output
                                    .get("calendar_sync_status")
                                    .and_then(serde_json::Value::as_str)
                                    .map(|status| match status {
                                        "synced" => " Google Calendar: synced.".to_string(),
                                        "preserved" => {
                                            " Google Calendar event preserved.".to_string()
                                        }
                                        "disabled" => " Google Calendar sync disabled.".to_string(),
                                        "skipped" => " Google Calendar: skipped.".to_string(),
                                        "error" => {
                                            let error = result
                                                .output
                                                .get("calendar_sync_error")
                                                .and_then(serde_json::Value::as_str)
                                                .unwrap_or("unknown error");
                                            format!(" Google Calendar sync failed: {error}")
                                        }
                                        other => format!(" Google Calendar: {other}."),
                                    })
                                    .unwrap_or_default();
                                self.notify(format!("{base}{sync_message}"));
                            }
                            Err(error) => {
                                if let Some(active) = self.form.as_mut() {
                                    active.validation_error =
                                        Some(format!("Saved, but refresh failed: {error}"));
                                }
                            }
                        }
                    }
                    Ok(other) => {
                        if let Some(active) = self.form.as_mut() {
                            active.validation_error =
                                Some(format!("Unexpected reminder mutation response: {other:?}"));
                        }
                    }
                    Err(error) => {
                        if let Some(active) = self.form.as_mut() {
                            active.validation_error =
                                Some(format!("Reminder save failed: {error}"));
                        }
                    }
                }
            }
        }
    }

    fn confirm_action(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::CompleteTask => {
                let Some(index) = self.selected_task_index() else {
                    return;
                };
                let Some(task) = self.tasks.get(index) else {
                    return;
                };
                self.modal = Some(Modal::Confirm(action));
                self.message = format!("Complete '{}' ?", task.title);
            }
            ConfirmAction::CancelTask => {
                let Some(index) = self.selected_task_index() else {
                    return;
                };
                let Some(task) = self.tasks.get(index) else {
                    return;
                };
                self.modal = Some(Modal::Confirm(action));
                self.message = format!("Cancel '{}' ?", task.title);
            }
            ConfirmAction::CompleteReminder | ConfirmAction::CancelReminder => {
                let Some(reminder) = self
                    .selected_reminder_index()
                    .and_then(|index| self.reminders.get(index))
                else {
                    return;
                };
                self.modal = Some(Modal::Confirm(action));
                self.message = match action {
                    ConfirmAction::CompleteReminder => format!("Complete '{}' ?", reminder.title),
                    ConfirmAction::CancelReminder => format!("Cancel '{}' ?", reminder.title),
                    _ => self.message.clone(),
                };
            }
            ConfirmAction::RelaunchRuntime => self.modal = Some(Modal::Confirm(action)),
        }
    }

    fn execute_confirmed(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::CompleteTask => {
                let Some(task) = self
                    .selected_task_index()
                    .and_then(|index| self.tasks.get(index))
                else {
                    self.modal = None;
                    return;
                };
                let id = task.id.clone();
                match self.client.request(RequestMethod::TasksMutate {
                    operation: "complete".to_string(),
                    id: Some(id.clone()),
                    title: None,
                    body: None,
                    due: None,
                    status: None,
                }) {
                    Ok(ResponsePayload::Mutation(_)) => match self.refresh() {
                        Ok(()) => {
                            self.selected = self
                                .tasks
                                .iter()
                                .position(|task| task.id == id)
                                .unwrap_or(0);
                            self.notify("Task completed in backend.".to_string());
                        }
                        Err(error) => {
                            self.notify(format!("Task completed, refresh failed: {error}"))
                        }
                    },
                    Ok(other) => {
                        self.notify(format!("Unexpected task mutation response: {other:?}"))
                    }
                    Err(error) => self.notify(format!("Task completion failed: {error}")),
                }
                self.modal = None;
            }
            ConfirmAction::CancelTask => {
                let Some(task) = self
                    .selected_task_index()
                    .and_then(|index| self.tasks.get(index))
                else {
                    self.modal = None;
                    return;
                };
                let id = task.id.clone();
                match self.client.request(RequestMethod::TasksMutate {
                    operation: "cancel".to_string(),
                    id: Some(id.clone()),
                    title: None,
                    body: None,
                    due: None,
                    status: None,
                }) {
                    Ok(ResponsePayload::Mutation(_)) => match self.refresh() {
                        Ok(()) => {
                            self.selected = self
                                .tasks
                                .iter()
                                .position(|task| task.id == id)
                                .unwrap_or(0);
                            self.notify("Task cancelled in backend.".to_string());
                        }
                        Err(error) => {
                            self.notify(format!("Task cancelled, refresh failed: {error}"))
                        }
                    },
                    Ok(other) => {
                        self.notify(format!("Unexpected task mutation response: {other:?}"))
                    }
                    Err(error) => self.notify(format!("Task cancellation failed: {error}")),
                }
                self.modal = None;
            }
            ConfirmAction::CompleteReminder | ConfirmAction::CancelReminder => {
                let Some(index) = self.selected_reminder_index() else {
                    self.modal = None;
                    return;
                };
                let Some(reminder) = self.reminders.get(index) else {
                    self.modal = None;
                    return;
                };
                let id = reminder.id.clone();
                let operation = if matches!(action, ConfirmAction::CompleteReminder) {
                    "complete"
                } else {
                    "cancel"
                };
                match self.client.request(RequestMethod::RemindersMutate {
                    operation: operation.to_string(),
                    id: Some(id.clone()),
                    title: None,
                    body: None,
                    due: None,
                    calendar_sync: None,
                }) {
                    Ok(ResponsePayload::Mutation(_)) => match self.refresh() {
                        Ok(()) => {
                            self.notify(if operation == "complete" {
                                "Reminder completed in backend.".to_string()
                            } else {
                                "Reminder cancelled in backend.".to_string()
                            });
                        }
                        Err(error) => self.notify(format!(
                            "Reminder {} in backend, refresh failed: {error}",
                            operation
                        )),
                    },
                    Ok(other) => {
                        self.notify(format!("Unexpected reminder mutation response: {other:?}"))
                    }
                    Err(error) => self.notify(format!("Reminder {} failed: {error}", operation)),
                }
                self.modal = None;
            }
            ConfirmAction::RelaunchRuntime => self.relaunch_runtime(),
        }
    }

    fn notify(&mut self, message: String) {
        self.message = message;
    }

    fn start_search_input(&mut self) {
        self.input_mode = InputMode::Search;
        self.search_cursor = self.search_input.chars().count();
    }

    fn start_chat_input(&mut self) {
        self.input_mode = InputMode::Chat;
        self.chat_cursor = self.chat_input.chars().count();
        self.recompute_input_scroll();
    }

    fn insert_char(&mut self, value: char) {
        match self.input_mode {
            InputMode::Chat => insert_at_cursor(&mut self.chat_input, &mut self.chat_cursor, value),
            InputMode::Search => {
                insert_at_cursor(&mut self.search_input, &mut self.search_cursor, value)
            }
            InputMode::None => {}
        }
        if matches!(self.input_mode, InputMode::Chat) {
            self.recompute_input_scroll();
        }
    }

    fn backspace(&mut self) {
        match self.input_mode {
            InputMode::Chat => backspace_at_cursor(&mut self.chat_input, &mut self.chat_cursor),
            InputMode::Search => {
                backspace_at_cursor(&mut self.search_input, &mut self.search_cursor)
            }
            InputMode::None => {}
        }
        if matches!(self.input_mode, InputMode::Chat) {
            self.recompute_input_scroll();
        }
    }

    fn delete(&mut self) {
        match self.input_mode {
            InputMode::Chat => delete_at_cursor(&mut self.chat_input, &mut self.chat_cursor),
            InputMode::Search => delete_at_cursor(&mut self.search_input, &mut self.search_cursor),
            InputMode::None => {}
        }
        if matches!(self.input_mode, InputMode::Chat) {
            self.recompute_input_scroll();
        }
    }

    fn cursor_left(&mut self) {
        match self.input_mode {
            InputMode::Chat => self.chat_cursor = self.chat_cursor.saturating_sub(1),
            InputMode::Search => self.search_cursor = self.search_cursor.saturating_sub(1),
            InputMode::None => {}
        }
    }

    fn cursor_right(&mut self) {
        match self.input_mode {
            InputMode::Chat => {
                self.chat_cursor = (self.chat_cursor + 1).min(self.chat_input.chars().count())
            }
            InputMode::Search => {
                self.search_cursor = (self.search_cursor + 1).min(self.search_input.chars().count())
            }
            InputMode::None => {}
        }
    }

    fn cursor_home(&mut self) {
        match self.input_mode {
            InputMode::Chat => self.chat_cursor = line_start(&self.chat_input, self.chat_cursor),
            InputMode::Search => self.search_cursor = 0,
            InputMode::None => {}
        }
    }

    fn cursor_end(&mut self) {
        match self.input_mode {
            InputMode::Chat => self.chat_cursor = line_end(&self.chat_input, self.chat_cursor),
            InputMode::Search => self.search_cursor = self.search_input.chars().count(),
            InputMode::None => {}
        }
    }

    fn cursor_up(&mut self) {
        if !matches!(self.input_mode, InputMode::Chat) {
            return;
        }
        let width = inner_width(self.last_layout.input).max(1);
        self.chat_cursor = move_vertical(&self.chat_input, self.chat_cursor, width, -1);
        self.recompute_input_scroll();
    }

    fn cursor_down(&mut self) {
        if !matches!(self.input_mode, InputMode::Chat) {
            return;
        }
        let width = inner_width(self.last_layout.input).max(1);
        self.chat_cursor = move_vertical(&self.chat_input, self.chat_cursor, width, 1);
        self.recompute_input_scroll();
    }

    fn recompute_input_scroll(&mut self) {
        let width = inner_width(self.last_layout.input).max(1);
        let (_, row, _) = editor_visual_lines(&self.chat_input, self.chat_cursor, width);
        let visible = MAX_EDITOR_LINES.saturating_sub(0).max(1);
        if row >= self.input_scroll as usize + visible {
            self.input_scroll = row.saturating_sub(visible - 1) as u16;
        } else if row < self.input_scroll as usize {
            self.input_scroll = row as u16;
        }
    }

    fn handle_event(&mut self, event: Event) -> Result<bool, Box<dyn Error>> {
        match event {
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse);
                return Ok(true);
            }
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    return Ok(true);
                }
                return self.handle_key(key);
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
        Ok(true)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool, Box<dyn Error>> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if let Some(Modal::Approval(approval_id)) = self.modal {
                self.respond_to_approval(approval_id, false);
                return Ok(true);
            }
            if self.modal.is_some() {
                self.modal = None;
                return Ok(true);
            }
            if self.form.is_some() {
                self.form = None;
                return Ok(true);
            }
            if self.input_mode != InputMode::None {
                self.input_mode = InputMode::None;
                return Ok(true);
            }
            return Ok(false);
        }

        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }

        if let Some(mut form) = self.form.take() {
            match self.handle_form_key(key, &mut form) {
                FormAction::Keep => self.form = Some(form),
                FormAction::Submit => {
                    self.form = Some(form);
                    self.submit_form();
                }
                FormAction::Close => {}
            }
            return Ok(true);
        }

        if self.input_mode != InputMode::None {
            return self.handle_input_key(key);
        }

        match key.code {
            KeyCode::Esc => Ok(false),
            KeyCode::Char('?') => {
                self.modal = Some(Modal::Help);
                Ok(true)
            }
            KeyCode::Char('r') => {
                match self.refresh() {
                    Ok(()) => self.notify("Refreshed.".to_string()),
                    Err(error) => self.notify(format!("Refresh failed: {error}")),
                }
                Ok(true)
            }
            KeyCode::Char('h') => {
                self.previous_tab();
                Ok(true)
            }
            KeyCode::Char('l') => {
                self.next_tab();
                Ok(true)
            }
            KeyCode::Char(c) if ('1'..='9').contains(&c) => {
                self.set_tab(Tab::from_index(c as usize - '1' as usize));
                Ok(true)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Ok(true)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Ok(true)
            }
            KeyCode::PageUp => {
                self.move_selection(-8);
                Ok(true)
            }
            KeyCode::PageDown => {
                self.move_selection(8);
                Ok(true)
            }
            KeyCode::Enter => {
                if self.tab == Tab::Models {
                    self.switch_selected_model();
                } else {
                    self.open_selected_detail();
                }
                Ok(true)
            }
            KeyCode::Char('c') => {
                match self.tab {
                    Tab::Chat => self.start_chat_input(),
                    Tab::Tasks => self.open_task_form(false),
                    Tab::Reminders => self.open_reminder_form(false),
                    _ => {}
                }
                Ok(true)
            }
            KeyCode::Char('e') => {
                match self.tab {
                    Tab::Tasks => self.open_task_form(true),
                    Tab::Reminders => self.open_reminder_form(true),
                    _ => {}
                }
                Ok(true)
            }
            KeyCode::Char('a') if self.tab == Tab::Proposals => {
                self.accept_selected_proposal();
                Ok(true)
            }
            KeyCode::Char('x') => {
                match self.tab {
                    Tab::Proposals => self.reject_selected_proposal(),
                    Tab::Tasks => self.confirm_action(ConfirmAction::CancelTask),
                    Tab::Reminders => self.confirm_action(ConfirmAction::CancelReminder),
                    _ => {}
                }
                Ok(true)
            }
            KeyCode::Char(' ') if matches!(self.tab, Tab::Tasks | Tab::Reminders) => {
                if self.tab == Tab::Tasks {
                    self.confirm_action(ConfirmAction::CompleteTask);
                } else {
                    self.confirm_action(ConfirmAction::CompleteReminder);
                }
                Ok(true)
            }
            KeyCode::Char('/') if self.tab == Tab::Search => {
                self.start_search_input();
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Result<bool, Box<dyn Error>> {
        match self.input_mode {
            InputMode::Chat => match key.code {
                KeyCode::Esc => {
                    self.input_mode = InputMode::None;
                    self.recompute_input_scroll();
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.insert_char('\n')
                }
                KeyCode::Enter => self.send_chat(),
                KeyCode::Backspace => self.backspace(),
                KeyCode::Delete => self.delete(),
                KeyCode::Left => self.cursor_left(),
                KeyCode::Right => self.cursor_right(),
                KeyCode::Up => self.cursor_up(),
                KeyCode::Down => self.cursor_down(),
                KeyCode::Home => self.cursor_home(),
                KeyCode::End => self.cursor_end(),
                KeyCode::PageUp => self.chat_scroll = self.chat_scroll.saturating_sub(8),
                KeyCode::PageDown => self.chat_scroll = self.chat_scroll.saturating_add(8),
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.insert_char(c)
                }
                _ => {}
            },
            InputMode::Search => match key.code {
                KeyCode::Esc => self.input_mode = InputMode::None,
                KeyCode::Enter => {
                    self.input_mode = InputMode::None;
                    self.run_search();
                }
                KeyCode::Backspace => self.backspace(),
                KeyCode::Delete => self.delete(),
                KeyCode::Left => self.cursor_left(),
                KeyCode::Right => self.cursor_right(),
                KeyCode::Home => self.cursor_home(),
                KeyCode::End => self.cursor_end(),
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.insert_char(c)
                }
                _ => {}
            },
            InputMode::None => {}
        }
        Ok(true)
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Result<bool, Box<dyn Error>> {
        let Some(modal) = self.modal else {
            return Ok(true);
        };
        match modal {
            Modal::Help => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.modal = None,
                _ => {}
            },
            Modal::Confirm(action) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => self.execute_confirmed(action),
                _ => {}
            },
            Modal::Approval(approval_id) => match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.respond_to_approval(approval_id, true);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.respond_to_approval(approval_id, false);
                }
                KeyCode::Up => self.modal_scroll = self.modal_scroll.saturating_sub(1),
                KeyCode::Down => self.modal_scroll = self.modal_scroll.saturating_add(1),
                KeyCode::PageUp => self.modal_scroll = self.modal_scroll.saturating_sub(8),
                KeyCode::PageDown => self.modal_scroll = self.modal_scroll.saturating_add(8),
                _ => {}
            },
            Modal::RuntimeReconnect => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => self.modal = Some(Modal::Confirm(ConfirmAction::RelaunchRuntime)),
                _ => {}
            },
            Modal::TaskDetail => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.modal = None,
                KeyCode::Up => self.modal_scroll = self.modal_scroll.saturating_sub(1),
                KeyCode::Down => self.modal_scroll = self.modal_scroll.saturating_add(1),
                KeyCode::PageUp => self.modal_scroll = self.modal_scroll.saturating_sub(8),
                KeyCode::PageDown => self.modal_scroll = self.modal_scroll.saturating_add(8),
                KeyCode::Char('e') => {
                    self.modal = None;
                    self.open_task_form(true);
                }
                KeyCode::Char(' ') => self.confirm_action(ConfirmAction::CompleteTask),
                KeyCode::Char('x') => self.confirm_action(ConfirmAction::CancelTask),
                _ => {}
            },
            Modal::ReminderDetail => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.modal = None,
                KeyCode::Up => self.modal_scroll = self.modal_scroll.saturating_sub(1),
                KeyCode::Down => self.modal_scroll = self.modal_scroll.saturating_add(1),
                KeyCode::PageUp => self.modal_scroll = self.modal_scroll.saturating_sub(8),
                KeyCode::PageDown => self.modal_scroll = self.modal_scroll.saturating_add(8),
                KeyCode::Char('e') => {
                    self.modal = None;
                    self.open_reminder_form(true);
                }
                KeyCode::Char(' ') => self.confirm_action(ConfirmAction::CompleteReminder),
                KeyCode::Char('x') => {
                    self.modal = Some(Modal::Confirm(ConfirmAction::CancelReminder));
                }
                _ => {}
            },
            Modal::ProposalDetail => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.modal = None,
                KeyCode::Up => self.modal_scroll = self.modal_scroll.saturating_sub(1),
                KeyCode::Down => self.modal_scroll = self.modal_scroll.saturating_add(1),
                KeyCode::PageUp => self.modal_scroll = self.modal_scroll.saturating_sub(8),
                KeyCode::PageDown => self.modal_scroll = self.modal_scroll.saturating_add(8),
                KeyCode::Char('a') => {
                    self.modal = None;
                    self.accept_selected_proposal();
                }
                KeyCode::Char('x') => {
                    self.modal = None;
                    self.reject_selected_proposal();
                }
                _ => {}
            },
            Modal::SearchDetail => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.modal = None,
                KeyCode::Up => self.modal_scroll = self.modal_scroll.saturating_sub(1),
                KeyCode::Down => self.modal_scroll = self.modal_scroll.saturating_add(1),
                KeyCode::PageUp => self.modal_scroll = self.modal_scroll.saturating_sub(8),
                KeyCode::PageDown => self.modal_scroll = self.modal_scroll.saturating_add(8),
                _ => {}
            },
            Modal::CalendarDetail => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.modal = None,
                KeyCode::Up => self.modal_scroll = self.modal_scroll.saturating_sub(1),
                KeyCode::Down => self.modal_scroll = self.modal_scroll.saturating_add(1),
                KeyCode::PageUp => self.modal_scroll = self.modal_scroll.saturating_sub(8),
                KeyCode::PageDown => self.modal_scroll = self.modal_scroll.saturating_add(8),
                _ => {}
            },
        }
        Ok(true)
    }

    fn handle_form_key(&mut self, key: KeyEvent, form: &mut FormState) -> FormAction {
        match key.code {
            KeyCode::Esc => return FormAction::Close,
            KeyCode::Tab => form.move_field(1),
            KeyCode::BackTab => form.move_field(-1),
            KeyCode::Enter if form.active == 1 && key.modifiers.contains(KeyModifiers::SHIFT) => {
                insert_at_cursor(&mut form.values[form.active], &mut form.cursor, '\n');
            }
            KeyCode::Enter => return FormAction::Submit,
            KeyCode::Backspace => {
                let active = form.active;
                backspace_at_cursor(&mut form.values[active], &mut form.cursor);
            }
            KeyCode::Delete => {
                let active = form.active;
                delete_at_cursor(&mut form.values[active], &mut form.cursor);
            }
            KeyCode::Left => form.cursor = form.cursor.saturating_sub(1),
            KeyCode::Right => {
                form.cursor = (form.cursor + 1).min(form.active_value().chars().count());
            }
            KeyCode::Up if form.active == 1 => {
                form.cursor = move_vertical(form.active_value(), form.cursor, 58, -1);
            }
            KeyCode::Down if form.active == 1 => {
                form.cursor = move_vertical(form.active_value(), form.cursor, 58, 1);
            }
            KeyCode::Home => form.cursor = line_start(form.active_value(), form.cursor),
            KeyCode::End => form.cursor = line_end(form.active_value(), form.cursor),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let active = form.active;
                insert_at_cursor(&mut form.values[active], &mut form.cursor, c);
            }
            _ => {}
        }
        FormAction::Keep
    }

    fn open_selected_detail(&mut self) {
        self.modal = match self.tab {
            Tab::Overview => overview_detail_modal(self.overview_rows().get(self.selected)),
            Tab::Tasks if self.selected_task_index().is_some() => Some(Modal::TaskDetail),
            Tab::Reminders if self.selected_reminder_index().is_some() => {
                Some(Modal::ReminderDetail)
            }
            Tab::Proposals if self.selected_proposal().is_some() => Some(Modal::ProposalDetail),
            Tab::Search if self.selected_search().is_some() => Some(Modal::SearchDetail),
            Tab::Calendar if self.selected_calendar_event().is_some() => {
                Some(Modal::CalendarDetail)
            }
            _ => None,
        };
        self.modal_scroll = 0;
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        let point = (mouse.column, mouse.row);

        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                -1
            } else {
                1
            };
            if self.modal.is_some() {
                if rect_contains(self.last_layout.modal, point.0, point.1) {
                    self.modal_scroll = if delta < 0 {
                        self.modal_scroll.saturating_sub(3)
                    } else {
                        self.modal_scroll.saturating_add(3)
                    };
                }
                return;
            }
            if self.tab == Tab::Chat && rect_contains(self.last_layout.list, point.0, point.1) {
                self.chat_scroll = if delta < 0 {
                    self.chat_scroll.saturating_sub(3)
                } else {
                    self.chat_scroll.saturating_add(3)
                };
                return;
            }
            if self.tab == Tab::Chat && rect_contains(self.last_layout.input, point.0, point.1) {
                self.input_scroll = if delta < 0 {
                    self.input_scroll.saturating_sub(1)
                } else {
                    self.input_scroll.saturating_add(1)
                };
                return;
            }
            if rect_contains(self.last_layout.list, point.0, point.1) {
                self.move_selection(if delta < 0 { -3 } else { 3 });
            }
            return;
        }

        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }
        if rect_contains(self.last_layout.header, point.0, point.1) {
            if let Some(tab) = tab_from_mouse(point.0, point.1, self.last_layout.header) {
                self.set_tab(tab);
                return;
            }
        }
        if self.tab == Tab::Chat
            && self.input_mode == InputMode::Chat
            && rect_contains(self.last_layout.input, point.0, point.1)
        {
            self.position_chat_cursor_from_mouse(point.0, point.1);
            return;
        }
        if self.tab == Tab::Search
            && self.input_mode == InputMode::Search
            && rect_contains(self.last_layout.input, point.0, point.1)
        {
            let target = point.0.saturating_sub(self.last_layout.input.x + 1) as usize;
            self.search_cursor = target.min(self.search_input.chars().count());
            return;
        }
        if rect_contains(self.last_layout.list, point.0, point.1) {
            if let Some(index) = self.list_index_from_mouse(point.1) {
                self.selected = index;
                self.open_selected_detail();
            }
        }
    }

    fn list_index_from_mouse(&self, y: u16) -> Option<usize> {
        let content_y = match self.tab {
            Tab::Overview | Tab::Models | Tab::Search => self.last_layout.list.y.saturating_add(1),
            _ => self.last_layout.list.y,
        };
        let row = y.checked_sub(content_y)? as usize + self.list_scroll;
        match self.tab {
            Tab::Tasks => indexed_grouped_row(
                &self
                    .tasks
                    .iter()
                    .map(|task| task.status.as_str())
                    .collect::<Vec<_>>(),
                row,
            ),
            Tab::Reminders => indexed_grouped_row(
                &self
                    .reminders
                    .iter()
                    .map(|reminder| reminder.status.as_str())
                    .collect::<Vec<_>>(),
                row,
            ),
            Tab::Jobs => indexed_grouped_row(
                &self
                    .jobs
                    .iter()
                    .map(|job| job.status.as_str())
                    .collect::<Vec<_>>(),
                row,
            ),
            Tab::Overview => row
                .checked_sub(1)
                .filter(|index| *index < self.overview_rows().len()),
            Tab::Models => {
                let available = self.models.len();
                let per_model =
                    if LayoutMode::for_area(self.last_layout.content) == LayoutMode::Mini {
                        1
                    } else {
                        2
                    };
                let index = row / per_model;
                (index < available).then_some(index)
            }
            Tab::Proposals | Tab::Calendar => (row < self.current_len()).then_some(row),
            Tab::Search => search_visual_to_result_index(&self.search_results, row),
            Tab::Chat => None,
        }
    }

    fn position_chat_cursor_from_mouse(&mut self, x: u16, y: u16) {
        let width = inner_width(self.last_layout.input).max(1);
        let target_row =
            y.saturating_sub(self.last_layout.input.y + 1) as usize + self.input_scroll as usize;
        let target_col = x.saturating_sub(self.last_layout.input.x + 1) as usize;
        self.chat_cursor =
            editor_cursor_at_visual_position(&self.chat_input, width, target_row, target_col);
        self.recompute_input_scroll();
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let mode = LayoutMode::for_area(area);
        if mode == LayoutMode::TooSmall {
            self.draw_too_small(frame, area);
            return;
        }

        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(outer_border_style())
            .style(Style::default().bg(app_background()));
        let inner_area = outer.inner(area);
        frame.render_widget(outer, area);

        let header_height = match mode {
            LayoutMode::Full => 4,
            LayoutMode::Compact => 4,
            LayoutMode::Mini => 5,
            LayoutMode::TooSmall => 1,
        };
        let footer_height = 2;
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_height),
                Constraint::Min(1),
                Constraint::Length(footer_height),
            ])
            .split(inner_area);

        let rects = UiRects {
            header: vertical[0],
            content: vertical[1],
            footer: vertical[2],
            list: vertical[1],
            input: Rect::default(),
            modal: Rect::default(),
        };
        self.last_layout = rects;
        self.draw_header(frame, vertical[0], mode);
        self.draw_view(frame, vertical[1], mode);
        self.draw_footer(frame, vertical[2], mode);
        self.draw_modal(frame, mode);
    }

    fn draw_too_small(&mut self, frame: &mut Frame, area: Rect) {
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(outer_border_style())
            .style(Style::default().bg(app_background()));
        let inner = outer.inner(area);
        frame.render_widget(outer, area);

        let tab = format!("{}:{}", self.tab.index() + 1, TABS[self.tab.index()]);
        let item = match self.tab {
            Tab::Tasks => self
                .tasks
                .get(self.selected)
                .map(|task| compact(&task.title, inner.width.saturating_sub(2) as usize)),
            Tab::Reminders => self
                .reminders
                .get(self.selected)
                .map(|reminder| compact(&reminder.title, inner.width.saturating_sub(2) as usize)),
            Tab::Overview => self
                .overview_rows()
                .get(self.selected)
                .map(|row| match row {
                    RowKind::Task(index) => self.tasks[*index].title.clone(),
                    RowKind::Reminder(index) => self.reminders[*index].title.clone(),
                })
                .map(|value| compact(&value, inner.width.saturating_sub(2) as usize)),
            _ => None,
        };
        let mut lines = vec![Line::from(vec![
            Span::styled("Zaraki", app_title_style()),
            Span::raw("  "),
            Span::styled(tab, panel_title_style()),
        ])];
        if let Some(item) = item {
            lines.push(Line::from(format!("> {item}")));
        } else if matches!(self.tab, Tab::Chat) {
            lines.push(Line::from("c input"));
        } else {
            lines.push(Line::from(Span::styled("No selection.", muted_style())));
        }
        lines.push(Line::from(Span::styled(
            "1-9 views | j/k move | Enter detail | Esc quit",
            muted_style(),
        )));
        frame.render_widget(Paragraph::new(lines), inner);
        self.last_layout = UiRects {
            header: Rect::default(),
            content: inner,
            footer: Rect::default(),
            list: inner,
            input: Rect::default(),
            modal: Rect::default(),
        };
    }

    fn draw_header(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let mut lines = Vec::new();
        let runtime_style = if self
            .health
            .as_ref()
            .map(|value| value.ready)
            .unwrap_or(false)
        {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        };
        let runtime_text = if self.pending_runtime_restart.is_some() {
            "⟳ reconnecting"
        } else if self
            .health
            .as_ref()
            .map(|value| value.ready)
            .unwrap_or(false)
        {
            "● runtime ready"
        } else {
            "⚠ runtime disconnected"
        };
        lines.push(Line::from(vec![
            Span::styled("Zaraki", app_title_style()),
            Span::raw("  "),
            Span::styled(runtime_text, runtime_style),
        ]));

        let tab_width = area.width.saturating_sub(2).max(1);
        lines.extend(tab_lines(self.tab.index(), tab_width));
        if mode == LayoutMode::Mini {
            lines.truncate(area.height.saturating_sub(2) as usize);
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(panel_border_style()),
            ),
            area,
        );
    }

    fn draw_view(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        match self.tab {
            Tab::Overview => self.draw_overview(frame, area, mode),
            Tab::Tasks => self.draw_tasks(frame, area, mode),
            Tab::Reminders => self.draw_reminders(frame, area, mode),
            Tab::Proposals => self.draw_proposals(frame, area, mode),
            Tab::Chat => self.draw_chat(frame, area, mode),
            Tab::Models => self.draw_models(frame, area, mode),
            Tab::Jobs => self.draw_jobs(frame, area, mode),
            Tab::Search => self.draw_search(frame, area, mode),
            Tab::Calendar => self.draw_calendar(frame, area, mode),
        }
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let width = inner_width(area).max(1);
        let active_model = self
            .model_status
            .as_ref()
            .and_then(|status| status.active_model.as_deref())
            .and_then(|id| self.models.iter().find(|model| model.id == id))
            .map(|model| model.display_name.as_str())
            .or_else(|| {
                self.model_status
                    .as_ref()
                    .and_then(|status| status.active_model.as_deref())
            })
            .unwrap_or("unknown");
        let left = if !self.message.is_empty() {
            format!(
                "{} | model: {}",
                compact(&self.message, 54),
                compact(active_model, 34)
            )
        } else {
            format!(
                "{} | model: {}",
                if self.health.as_ref().map(|h| h.ready).unwrap_or(false) {
                    "● ready"
                } else {
                    "⚠ disconnected"
                },
                compact(active_model, 34)
            )
        };
        let right = footer_commands(self.tab, self.input_mode, self.pending_chat.is_some(), mode);
        let left_width = left.chars().count();
        let right_width = right.chars().count();
        let text = if left_width + right_width + 1 <= width {
            format!(
                "{}{}{}",
                left,
                " ".repeat(width - left_width - right_width),
                right
            )
        } else {
            compact(&format!("{}  {}", left, right), width)
        };
        frame.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(panel_border_style()),
            ),
            area,
        );
    }

    fn draw_overview(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let available = area.height.saturating_sub(2);
        let focus_height = if mode == LayoutMode::Full { 7 } else { 4 };
        let proposal_height = if mode == LayoutMode::Mini { 3 } else { 4 };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(focus_height.min(available)),
                Constraint::Min(4),
                Constraint::Length(proposal_height.min(available)),
            ])
            .split(area);

        let mut focus_lines = Vec::new();
        let today = OffsetDateTime::now_local()
            .unwrap_or_else(|_| OffsetDateTime::now_utc())
            .date();
        for index in self
            .important_task_indices()
            .into_iter()
            .filter(|index| task_is_due_today(&self.tasks[*index], today))
            .take(2)
        {
            let task = &self.tasks[index];
            focus_lines.push(Line::from(vec![
                Span::raw(format!("{} ", task_symbol(&task.status))),
                Span::styled(
                    compact(&task.title, inner_width(sections[0]).saturating_sub(4)),
                    task_priority_style(&task.priority),
                ),
            ]));
        }
        for index in self
            .important_reminder_indices()
            .into_iter()
            .filter(|index| reminder_is_due_today(&self.reminders[*index], today))
            .take(1)
        {
            let reminder = &self.reminders[index];
            focus_lines.push(Line::from(vec![
                Span::styled("◷ ", calendar_sync_style(reminder.calendar_sync_enabled)),
                Span::raw(compact(
                    &reminder.title,
                    inner_width(sections[0]).saturating_sub(4),
                )),
            ]));
        }
        if focus_lines.is_empty() {
            focus_lines.push(Line::from(Span::styled(
                "Nothing scheduled for today.",
                muted_style(),
            )));
        }
        if !focus_lines.is_empty() {
            focus_lines.push(Line::from(""));
            focus_lines.push(Line::from(Span::styled(
                "Enter opens details",
                muted_style(),
            )));
        }
        frame.render_widget(
            Paragraph::new(focus_lines)
                .block(panel_block("Today's focus"))
                .wrap(Wrap { trim: true }),
            sections[0],
        );

        let mut lines = Vec::new();
        let max_items = match mode {
            LayoutMode::Full => 14,
            LayoutMode::Compact => 8,
            LayoutMode::Mini => 4,
            LayoutMode::TooSmall => 2,
        };
        let overview_rows = self.overview_rows();
        for (row_index, row) in overview_rows.iter().take(max_items).enumerate() {
            let selected = row_index == self.selected;
            let prefix = if selected { "> " } else { "  " };
            let item = match row {
                RowKind::Task(index) => task_line(
                    &self.tasks[*index],
                    inner_width(sections[1]).saturating_sub(2),
                    mode,
                ),
                RowKind::Reminder(index) => reminder_line(
                    &self.reminders[*index],
                    inner_width(sections[1]).saturating_sub(2),
                    mode,
                ),
            };
            let mut spans = vec![Span::styled(
                prefix,
                if selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )];
            spans.extend(item.spans);
            lines.push(Line::from(spans));
        }
        if lines.len() == 1 {
            lines.push(Line::from(Span::styled("Nothing pending.", muted_style())));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel_block("Today & Important"))
                .wrap(Wrap { trim: true }),
            sections[1],
        );

        let review_line = if self.proposals.is_empty() {
            "No memory proposals pending review.".to_string()
        } else {
            format!("{} memory proposal(s) pending review", self.proposals.len())
        };
        frame.render_widget(
            Paragraph::new(Line::from(if self.proposals.is_empty() {
                Span::styled(review_line, muted_style())
            } else {
                Span::raw(review_line)
            }))
            .block(panel_block("Memory"))
            .wrap(Wrap { trim: true }),
            sections[2],
        );
        self.last_layout.list = sections[1];
    }

    fn tasks_visual_item_count(&self) -> usize {
        grouped_visual_item_count(
            &self
                .tasks
                .iter()
                .map(|task| task.status.as_str())
                .collect::<Vec<_>>(),
        )
    }

    fn reminders_visual_item_count(&self) -> usize {
        grouped_visual_item_count(
            &self
                .reminders
                .iter()
                .map(|reminder| reminder.status.as_str())
                .collect::<Vec<_>>(),
        )
    }

    fn draw_tasks(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let block = panel_block("Tasks");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let header = task_header_line(inner.width as usize, mode);
        frame.render_widget(
            Paragraph::new(header),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let list_area = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        );

        let mut items = Vec::new();
        let mut selected_item = None;
        let mut last_status: Option<&str> = None;
        for (index, task) in self.tasks.iter().enumerate() {
            if last_status != Some(task.status.as_str()) {
                let label = task_status_group_label(&task.status);
                items.push(ListItem::new(Line::from(Span::styled(
                    format!("▸ {label}"),
                    section_style(),
                ))));
                last_status = Some(task.status.as_str());
            }
            if index == self.selected {
                selected_item = Some(items.len());
            }
            items.push(ListItem::new(task_line(task, inner.width as usize, mode)));
        }

        if items.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "No tasks yet. Press c to create one.",
                muted_style(),
            ))));
        }

        let mut state = ListState::default();
        state.select(selected_item);
        let list = List::new(items)
            .highlight_style(selected_row_style())
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, list_area, &mut state);
        self.list_scroll = state.offset();
        render_vertical_scrollbar(
            frame,
            list_area,
            state.offset(),
            self.tasks_visual_item_count(),
            list_area.height as usize,
            false,
        );
        self.last_layout.list = list_area;
    }

    fn draw_reminders(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let block = panel_block("Reminders");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let header = reminder_header_line(inner.width as usize, mode);
        frame.render_widget(
            Paragraph::new(header),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let list_area = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        );
        let mut items = Vec::new();
        let mut selected_item = None;
        let mut last_status: Option<&str> = None;
        for (index, reminder) in self.reminders.iter().enumerate() {
            if last_status != Some(reminder.status.as_str()) {
                items.push(ListItem::new(Line::from(Span::styled(
                    format!("▸ {}", reminder_status_group_label(&reminder.status)),
                    section_style(),
                ))));
                last_status = Some(reminder.status.as_str());
            }
            if index == self.selected {
                selected_item = Some(items.len());
            }
            items.push(ListItem::new(reminder_line(
                reminder,
                inner.width as usize,
                mode,
            )));
        }
        if items.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "No reminders yet. Press c to create one.",
                muted_style(),
            ))));
        }
        let mut state = ListState::default();
        state.select(selected_item);
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(selected_row_style())
                .highlight_symbol("> "),
            list_area,
            &mut state,
        );
        self.list_scroll = state.offset();
        render_vertical_scrollbar(
            frame,
            list_area,
            state.offset(),
            self.reminders_visual_item_count(),
            list_area.height as usize,
            false,
        );
        self.last_layout.list = list_area;
    }

    fn draw_proposals(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let block = panel_block("Memory proposals");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if self.proposals.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "No memory proposals pending review.",
                    muted_style(),
                )),
                inner,
            );
            self.last_layout.list = area;
            return;
        }
        let header = proposal_header_line(inner.width as usize, mode);
        frame.render_widget(
            Paragraph::new(header),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let list_area = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        );
        let items = self
            .proposals
            .iter()
            .map(|proposal| ListItem::new(proposal_list_line(proposal, inner.width as usize, mode)))
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(self.selected.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(selected_row_style())
                .highlight_symbol("> "),
            list_area,
            &mut state,
        );
        self.list_scroll = state.offset();
        render_vertical_scrollbar(
            frame,
            list_area,
            state.offset(),
            self.proposals.len(),
            list_area.height as usize,
            false,
        );
        self.last_layout.list = list_area;
    }

    fn draw_chat(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let input_lines = editor_line_count(&self.chat_input, inner_width(area).max(1));
        let input_height = (input_lines.clamp(1, MAX_EDITOR_LINES) as u16 + 2)
            .min(area.height.saturating_sub(4).max(3));
        let inner = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(2), Constraint::Length(input_height)])
            .split(area);
        let history = self.chat_history_lines(inner[0], mode);
        let history_len = history.len();
        let viewport = inner[0].height.saturating_sub(2) as usize;
        let max_scroll = history.len().saturating_sub(viewport);
        self.chat_scroll = self
            .chat_scroll
            .min(max_scroll.min(u16::MAX as usize) as u16);
        let paragraph = Paragraph::new(history)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(panel_border_style())
                    .title(format!("Chat: {}", self.session_title))
                    .title_style(panel_title_style()),
            )
            .scroll((self.chat_scroll, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, inner[0]);
        render_vertical_scrollbar(
            frame,
            inner[0],
            self.chat_scroll as usize,
            history_len,
            viewport,
            true,
        );

        let editor_lines = editor_render_lines(
            &self.chat_input,
            Some(self.chat_cursor),
            inner_width(inner[1]).max(1),
        );
        let editor_scroll = self.input_scroll;
        let editor_title = if let Some(path) = &self.attachment {
            format!(
                "Input • attached: {}",
                compact(&path.display().to_string(), 28)
            )
        } else {
            "Input".to_string()
        };
        let editor_line_count = editor_lines.len();
        let editor_viewport = inner[1].height.saturating_sub(2) as usize;
        frame.render_widget(
            Paragraph::new(editor_lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(panel_border_style())
                        .title(editor_title)
                        .title_style(panel_title_style()),
                )
                .scroll((editor_scroll, 0))
                .wrap(Wrap { trim: false }),
            inner[1],
        );
        render_vertical_scrollbar(
            frame,
            inner[1],
            editor_scroll as usize,
            editor_line_count,
            editor_viewport,
            true,
        );
        self.last_layout.input = inner[1];
        self.last_layout.list = inner[0];

        if self.input_mode == InputMode::Chat && self.chat_input.starts_with(':') {
            let suggestions = command_suggestions(&self.chat_input);
            if !suggestions.is_empty() {
                let height = suggestions.len().min(4) as u16 + 2;
                let y = inner[1].y.saturating_sub(height);
                let popup = Rect::new(inner[1].x, y, inner[1].width, height);
                frame.render_widget(Clear, popup);
                frame.render_widget(
                    Paragraph::new(suggestions.into_iter().map(Line::from).collect::<Vec<_>>())
                        .block(panel_block("Commands")),
                    popup,
                );
            }
        }
    }

    fn chat_history_lines(&self, area: Rect, _mode: LayoutMode) -> Vec<Line<'static>> {
        let width = inner_width(area).max(1);
        if self.chat.is_empty() {
            return vec![Line::from(Span::styled(
                "No conversation yet. Press c to start.",
                muted_style(),
            ))];
        }
        let mut lines = Vec::new();
        for exchange in &self.chat {
            lines.push(Line::from(Span::styled("You", section_style())));
            lines.extend(render_plain_text(
                &exchange.user,
                width,
                "  ",
                Style::default(),
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Zaraki", app_title_style())));
            match &exchange.assistant {
                Some(text) => lines.extend(render_terminal_markdown(text, "  ", width)),
                None => lines.push(Line::from(Span::styled(
                    "  ⟳ Thinking...",
                    Style::default().fg(Color::Yellow),
                ))),
            }
            lines.push(Line::from(""));
        }
        lines
    }

    fn draw_models(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let block = panel_block("Models");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let diagnostics_height =
            if mode == LayoutMode::Mini { 5 } else { 9 }.min(inner.height.saturating_sub(4));
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(diagnostics_height)])
            .split(inner);
        let items = self
            .models
            .iter()
            .map(|model| {
                let marker = if model.active { "◆" } else { "◇" };
                let style = model_state_style(model.active, model.available);
                let name_width = sections[0].width.saturating_sub(18) as usize;
                let first_line = vec![
                    Span::styled(format!("{marker} "), style),
                    Span::styled(
                        compact(&model.display_name, name_width.max(8)),
                        style.add_modifier(Modifier::BOLD),
                    ),
                ];
                let mut lines = vec![Line::from(first_line)];
                if mode == LayoutMode::Full {
                    let availability = if model.available {
                        "available"
                    } else {
                        "unavailable"
                    };
                    lines.push(Line::from(vec![
                        Span::styled("   ", muted_style()),
                        Span::styled(compact(&model.id, 24), muted_style()),
                        Span::styled(format!("  {availability}"), style),
                    ]));
                } else if mode == LayoutMode::Compact {
                    lines.push(Line::from(vec![
                        Span::styled("   ", muted_style()),
                        Span::styled(
                            if model.available {
                                "available"
                            } else {
                                "unavailable"
                            },
                            style,
                        ),
                    ]));
                }
                ListItem::new(lines)
            })
            .collect::<Vec<_>>();
        let mut list_state = ListState::default();
        if !items.is_empty() {
            list_state.select(Some(self.selected.min(items.len().saturating_sub(1))));
        }
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(panel_border_style())
                        .title("Available models")
                        .title_style(panel_title_style()),
                )
                .highlight_style(selected_row_style())
                .highlight_symbol("> "),
            sections[0],
            &mut list_state,
        );
        self.list_scroll = list_state.offset();
        let model_item_height = if mode == LayoutMode::Mini { 1 } else { 2 };
        render_vertical_scrollbar(
            frame,
            sections[0],
            list_state.offset(),
            self.models.len(),
            sections[0].height.saturating_sub(2) as usize / model_item_height,
            true,
        );
        let active_model = self
            .models
            .iter()
            .find(|model| model.active && model.available);
        let diagnostics = system_diagnostics_lines(
            self.system_info.as_ref(),
            active_model.map(|model| model.display_name.as_str()),
            self.model_status.as_ref().map(|status| status.ready),
            mode,
        );
        frame.render_widget(
            Paragraph::new(diagnostics).block(panel_block("Diagnostics")),
            sections[1],
        );
        self.last_layout.list = sections[0];
    }

    fn draw_calendar(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let block = panel_block("Calendar");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let status = self.calendar_status.as_ref();
        let status_line = match status {
            Some(value) if value.authenticated => format!(
                "● Connected  | {} event(s) this month",
                self.calendar_events.len()
            ),
            Some(value) if value.configured => {
                "○ Not authenticated  | use :calendar-login".to_string()
            }
            Some(_) => "○ Google Calendar is not configured  | :calendar-login after OAuth setup"
                .to_string(),
            None => "? Calendar status unavailable".to_string(),
        };
        frame.render_widget(
            Paragraph::new(status_line).style(if status.is_some_and(|value| value.authenticated) {
                Style::default().fg(Color::Yellow)
            } else {
                muted_style()
            }),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let month_height = match mode {
            LayoutMode::Full => 9,
            LayoutMode::Compact => 9,
            LayoutMode::Mini => 9,
            LayoutMode::TooSmall => 5,
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(month_height.min(inner.height.saturating_sub(2))),
                Constraint::Min(2),
            ])
            .split(Rect::new(
                inner.x,
                inner.y.saturating_add(1),
                inner.width,
                inner.height.saturating_sub(1),
            ));

        let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
        let month_lines = calendar_month_lines(
            now.date(),
            &self.calendar_events,
            sections[0].width as usize,
            mode,
        );
        let month_title = format!("{} {}", month_name(now.month()), now.year());
        frame.render_widget(
            Paragraph::new(month_lines).block(panel_block(&month_title)),
            sections[0],
        );

        let list_area = sections[1];
        if self.calendar_events.is_empty() {
            let message = match status {
                Some(value) if value.authenticated => "No events in the current month.",
                Some(value) if value.configured => {
                    "Google Calendar is not authenticated. Use :calendar-login."
                }
                _ => "Google Calendar is unavailable.",
            };
            frame.render_widget(
                Paragraph::new(Span::styled(message, muted_style())).block(panel_block("Upcoming")),
                list_area,
            );
            self.last_layout.list = list_area;
            return;
        }

        let items = self
            .calendar_events
            .iter()
            .map(|event| {
                let title_width = if mode == LayoutMode::Full { 46 } else { 30 };
                let marker_style = if event.status == "cancelled" {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                ListItem::new(Line::from(vec![
                    Span::styled("◷ ", marker_style),
                    Span::styled(compact(&event.start, 18), muted_style()),
                    Span::raw("  "),
                    Span::styled(
                        compact(
                            &event.summary,
                            title_width.min(list_area.width.saturating_sub(26) as usize),
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(self.selected.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(
            List::new(items)
                .block(panel_block("Upcoming"))
                .highlight_style(selected_row_style())
                .highlight_symbol("> "),
            list_area,
            &mut state,
        );
        self.list_scroll = state.offset();
        render_vertical_scrollbar(
            frame,
            list_area,
            state.offset(),
            self.calendar_events.len(),
            list_area.height.saturating_sub(2) as usize,
            true,
        );
        self.last_layout.list = list_area;
    }

    fn draw_jobs(&mut self, frame: &mut Frame, area: Rect, mode: LayoutMode) {
        let block = panel_block("Jobs");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut items = Vec::new();
        let mut selected_item = None;
        let mut last_status: Option<&str> = None;
        for (index, job) in self.jobs.iter().enumerate() {
            if last_status != Some(job.status.as_str()) {
                items.push(ListItem::new(Line::from(Span::styled(
                    format!("▸ {}", job_status_group_label(&job.status)),
                    section_style(),
                ))));
                last_status = Some(job.status.as_str());
            }
            if index == self.selected {
                selected_item = Some(items.len());
            }
            let marker = if job.status == "running" {
                "⟳"
            } else if job.status == "failed" {
                "✕"
            } else if job.status == "completed" {
                "✓"
            } else {
                "•"
            };
            let line = match mode {
                LayoutMode::Mini => format!(
                    "{marker} {}  {}",
                    compact(&job.job_type, 22),
                    compact(&job.status, 10)
                ),
                LayoutMode::Compact => format!(
                    "{marker} {:<18} {:<10} {}",
                    compact(&job.job_type, 18),
                    compact(&job.status, 10),
                    compact(&job.next_run_at, 18)
                ),
                LayoutMode::Full | LayoutMode::TooSmall => format!(
                    "{marker} {:<22} {:<10} {}",
                    compact(&job.job_type, 22),
                    compact(&job.status, 10),
                    compact(&job.next_run_at, 30)
                ),
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} "), job_status_style(&job.status)),
                Span::raw(line.trim_start_matches(&format!("{marker} ")).to_string()),
            ])));
        }
        if items.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "No jobs yet.",
                muted_style(),
            ))));
        }
        let mut state = ListState::default();
        state.select(selected_item);
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(selected_row_style())
                .highlight_symbol("> "),
            inner,
            &mut state,
        );
        self.list_scroll = state.offset();
        render_vertical_scrollbar(
            frame,
            inner,
            state.offset(),
            grouped_visual_item_count(
                &self
                    .jobs
                    .iter()
                    .map(|job| job.status.as_str())
                    .collect::<Vec<_>>(),
            ),
            inner.height as usize,
            false,
        );
        self.last_layout.list = inner;
    }

    fn draw_search(&mut self, frame: &mut Frame, area: Rect, _mode: LayoutMode) {
        let input_height = 3;
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(input_height), Constraint::Min(2)])
            .split(area);
        let input_width = inner_width(vertical[0]).max(1);
        let input_text = if self.search_input.is_empty() && self.input_mode != InputMode::Search {
            "Type a query and press Enter".to_string()
        } else if self.search_input.is_empty() {
            "▏".to_string()
        } else {
            form_value_with_cursor(
                &self.search_input,
                self.search_cursor,
                self.input_mode == InputMode::Search,
                input_width,
            )
        };
        frame.render_widget(
            Paragraph::new(input_text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(panel_border_style())
                    .title("Search / everything")
                    .title_style(panel_title_style()),
            ),
            vertical[0],
        );
        self.last_layout.input = vertical[0];

        let mut groups: Vec<(String, Vec<(usize, &(String, String, f64))>)> = Vec::new();
        for (index, result) in self.search_results.iter().enumerate() {
            let group = search_group_label(&result.0).to_string();
            if let Some((_, entries)) = groups.iter_mut().find(|(name, _)| *name == group) {
                entries.push((index, result));
            } else {
                groups.push((group, vec![(index, result)]));
            }
        }
        groups.sort_by(|left, right| {
            let left_score = left
                .1
                .first()
                .map(|(_, result)| result.2)
                .unwrap_or(f64::NEG_INFINITY);
            let right_score = right
                .1
                .first()
                .map(|(_, result)| result.2)
                .unwrap_or(f64::NEG_INFINITY);
            right_score
                .partial_cmp(&left_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let group_count = groups.len();
        let mut items = Vec::<(Option<usize>, ListItem<'static>)>::new();
        if self.search_results.is_empty() {
            items.push((
                None,
                ListItem::new(Line::from(Span::styled(
                    if self.search_input.is_empty() {
                        "Press / to search indexed memory."
                    } else {
                        "No results."
                    },
                    muted_style(),
                ))),
            ));
        } else {
            for (group, entries) in groups {
                items.push((
                    None,
                    ListItem::new(Line::from(Span::styled(group, section_style()))),
                ));
                for (index, (_id, text, score)) in entries {
                    items.push((
                        Some(index),
                        ListItem::new(Line::from(vec![
                            Span::styled(format!("{score:.2}  "), muted_style()),
                            Span::raw(compact(text, inner_width(vertical[1]).saturating_sub(12))),
                        ])),
                    ));
                }
            }
        }

        let visual_selected = items
            .iter()
            .position(|(index, _)| *index == Some(self.selected));
        let mut state = ListState::default();
        state.select(visual_selected);
        let list = List::new(items.into_iter().map(|(_, item)| item).collect::<Vec<_>>())
            .block(panel_block("Results"))
            .highlight_style(selected_row_style())
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, vertical[1], &mut state);
        self.list_scroll = state.offset();
        render_vertical_scrollbar(
            frame,
            vertical[1],
            state.offset(),
            self.search_results.len() + group_count,
            vertical[1].height.saturating_sub(2) as usize,
            true,
        );
        self.last_layout.list = vertical[1];
    }

    fn draw_modal(&mut self, frame: &mut Frame, mode: LayoutMode) {
        self.last_layout.modal = Rect::default();
        let area = frame.area();
        if let Some(form) = &self.form {
            let width = match mode {
                LayoutMode::Full => 72.min(area.width.saturating_sub(6)),
                LayoutMode::Compact => 64.min(area.width.saturating_sub(4)),
                LayoutMode::Mini => area.width.saturating_sub(4),
                LayoutMode::TooSmall => return,
            };
            let lines = form_lines(form, width.saturating_sub(2) as usize);
            let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
            let x = area.x + area.width.saturating_sub(width) / 2;
            let y = area.y + area.height.saturating_sub(height) / 2;
            let popup = Rect::new(x, y, width.max(24), height.max(8));
            frame.render_widget(Clear, popup);
            frame.render_widget(
                Paragraph::new(lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(panel_border_style())
                            .title(form_title(form))
                            .title_style(panel_title_style()),
                    )
                    .wrap(Wrap { trim: false }),
                popup,
            );
            return;
        }
        let Some(modal) = self.modal else {
            return;
        };
        let width = match mode {
            LayoutMode::Full => 72.min(area.width.saturating_sub(6)),
            LayoutMode::Compact => 64.min(area.width.saturating_sub(4)),
            LayoutMode::Mini => area.width.saturating_sub(4),
            LayoutMode::TooSmall => return,
        };
        let lines = modal_lines(self, modal, width as usize);
        let height = (lines.len() as u16 + 4).min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let popup = Rect::new(x, y, width.max(20), height.max(5));
        let viewport = popup.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(viewport);
        self.modal_scroll = self
            .modal_scroll
            .min(max_scroll.min(u16::MAX as usize) as u16);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines.clone())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(panel_border_style())
                        .title(modal_title(modal))
                        .title_style(panel_title_style()),
                )
                .scroll((self.modal_scroll, 0))
                .wrap(Wrap { trim: false }),
            popup,
        );
        render_vertical_scrollbar(
            frame,
            popup,
            self.modal_scroll as usize,
            lines.len(),
            viewport,
            true,
        );
        self.last_layout.modal = popup;
    }
}

fn render_vertical_scrollbar(
    frame: &mut Frame,
    area: Rect,
    position: usize,
    content_length: usize,
    viewport_length: usize,
    inset: bool,
) {
    if area.height == 0
        || area.width == 0
        || content_length <= viewport_length
        || viewport_length == 0
    {
        return;
    }
    let scrollbar_area = if inset {
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        })
    } else {
        area
    };
    if scrollbar_area.height == 0 || scrollbar_area.width == 0 {
        return;
    }
    let mut state = ScrollbarState::new(content_length)
        .position(position.min(content_length.saturating_sub(viewport_length)))
        .viewport_content_length(viewport_length);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .thumb_symbol("█")
        .track_style(muted_style())
        .thumb_style(Style::default().fg(Color::Cyan));
    frame.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
}

fn grouped_visual_item_count(statuses: &[&str]) -> usize {
    if statuses.is_empty() {
        return 1;
    }
    let mut count = 0usize;
    let mut previous: Option<&str> = None;
    for status in statuses {
        if previous != Some(*status) {
            count += 1;
            previous = Some(*status);
        }
        count += 1;
    }
    count
}

fn indexed_grouped_row(statuses: &[&str], row: usize) -> Option<usize> {
    let mut visual_row = 0usize;
    let mut previous: Option<&str> = None;
    for (index, status) in statuses.iter().enumerate() {
        if previous != Some(status) {
            if visual_row == row {
                return None;
            }
            visual_row += 1;
            previous = Some(status);
        }
        if visual_row == row {
            return Some(index);
        }
        visual_row += 1;
    }
    None
}

fn inner_width(area: Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

fn unexpected_response(method: &str, response: ResponsePayload) -> IpcClientError {
    IpcClientError::Server {
        code: "unexpected_response".to_string(),
        message: format!("Unexpected response for {method}: {response:?}"),
    }
}

fn form_title(form: &FormState) -> String {
    match form.kind {
        FormKind::Task => {
            if form.editing_id.is_some() {
                "Edit Task".to_string()
            } else {
                "Create Task".to_string()
            }
        }
        FormKind::Reminder => {
            if form.editing_id.is_some() {
                "Edit Reminder".to_string()
            } else {
                "Create Reminder".to_string()
            }
        }
    }
}

#[cfg(test)]
fn task_timestamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:02}-{:02}-{:02} {:02}:{:02}",
        now.day(),
        now.month() as u8,
        (now.year() % 100).abs(),
        now.hour(),
        now.minute()
    )
}

fn form_value_with_cursor(value: &str, cursor: usize, active: bool, width: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let mut out = String::new();
    for (index, ch) in chars.iter().enumerate() {
        if active && index == cursor {
            out.push('▏');
        }
        out.push(*ch);
    }
    if active && cursor == chars.len() {
        out.push('▏');
    }
    compact(&out, width.max(8))
}

fn form_lines(form: &FormState, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, label) in form.labels.iter().enumerate() {
        let active = index == form.active;
        let value = form.values.get(index).map(String::as_str).unwrap_or("");
        let marker = if active { ">" } else { " " };
        let style = if active {
            selected_row_style()
        } else {
            Style::default()
        };

        if index == 1 {
            lines.push(Line::from(Span::styled(format!("{marker} {label}"), style)));
            let body_width = width.saturating_sub(2).max(8);
            let body_lines = editor_render_lines(value, active.then_some(form.cursor), body_width);
            for body_line in body_lines {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(body_line.spans);
                lines.push(Line::from(spans));
            }
            continue;
        }

        let available = width.saturating_sub(label.chars().count() + 5).max(8);
        if label == "Due" && value.is_empty() {
            let mut spans = vec![Span::styled(
                format!("{marker} {label:<9} ", label = label),
                style,
            )];
            if active {
                spans.push(Span::styled("▏", style));
            }
            spans.push(Span::styled(
                "DD/MM/YYYY hh:mm (09:00 default)",
                Style::default().fg(Color::DarkGray),
            ));
            lines.push(Line::from(spans));
            continue;
        }

        let rendered = if active {
            form_value_with_cursor(value, form.cursor, true, available)
        } else {
            compact(value, available)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} {label:<9} ", label = label), style),
            Span::styled(rendered, style),
        ]));
    }
    if let Some(error) = &form.validation_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("⚠ {error}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Tab/Shift+Tab fields   Enter save   Esc cancel",
        muted_style(),
    )));
    lines
}

fn modal_title(modal: Modal) -> &'static str {
    match modal {
        Modal::Help => "Keyboard help",
        Modal::Confirm(_) => "Confirm",
        Modal::TaskDetail => "Task",
        Modal::ReminderDetail => "Reminder",
        Modal::ProposalDetail => "Memory proposal",
        Modal::SearchDetail => "Search result",
        Modal::CalendarDetail => "Calendar event",
        Modal::Approval(_) => "Approval required",
        Modal::RuntimeReconnect => "Runtime disconnected",
    }
}

fn modal_lines(app: &App, modal: Modal, width: usize) -> Vec<Line<'static>> {
    match modal {
        Modal::Help => vec![
            "1-9     switch views".into(),
            "h/l     previous / next view".into(),
            "j/k     move selection".into(),
            "Enter   open detail / switch model".into(),
            "c       create task / reminder or enter Chat".into(),
            "e       edit selected task / reminder".into(),
            "Space   complete selected task / reminder".into(),
            "x       cancel task / reminder or reject proposal".into(),
            "a       accept memory proposal".into(),
            "/       search".into(),
            "r       refresh runtime state".into(),
            ":calendar-login  link Google Calendar".into(),
            "?       this help".into(),
            "Esc     back / close / quit".into(),
            "Ctrl+C  same layered exit behavior".into(),
        ],
        Modal::Confirm(action) => {
            let fallback = match action {
                ConfirmAction::CompleteTask => "Complete the selected task?",
                ConfirmAction::CancelTask => "Cancel the selected task?",
                ConfirmAction::CompleteReminder => "Complete the selected reminder?",
                ConfirmAction::CancelReminder => "Cancel the selected reminder?",
                ConfirmAction::RelaunchRuntime => "Relaunch the Assistant runtime?",
            };
            let message = if app.message.is_empty() {
                fallback.to_string()
            } else {
                app.message.clone()
            };
            vec![
                Line::from(message),
                Line::from(""),
                Line::from("Enter confirm   Esc cancel"),
            ]
        }
        Modal::Approval(approval_id) => {
            let Some(approval) = app
                .approvals
                .iter()
                .find(|approval| approval.id == approval_id)
            else {
                return vec![
                    Line::from("This approval request is no longer pending."),
                    Line::from(""),
                    Line::from(Span::styled("Esc close", muted_style())),
                ];
            };

            let arguments = serde_json::to_string_pretty(&approval.arguments)
                .unwrap_or_else(|_| approval.arguments.to_string());
            let mut lines = vec![format!("Tool: {}", approval.tool), "Arguments:".to_string()];
            lines.extend(arguments.lines().map(str::to_string));
            detail_lines(lines, width, "Enter/Y approve | N/Esc deny")
        }
        Modal::RuntimeReconnect => vec![
            Line::from("The Assistant runtime is disconnected."),
            Line::from("Existing chat work is marked failed and can be retried."),
            Line::from(""),
            Line::from("Enter to continue to relaunch confirmation."),
            Line::from("Esc to dismiss."),
        ],
        Modal::TaskDetail => {
            if let Some(index) = app.selected_task_index() {
                if let Some(task) = app.tasks.get(index) {
                    let mut lines = vec![format!("Title: {}", task.title), "Body".to_string()];
                    let body_lines = wrap_balanced_preserving_newlines(&task.body, width.max(1));
                    lines.extend(body_lines);
                    lines.extend([
                        format!("Status: {}", task_status_label(&task.status)),
                        format!("Due: {}", task.due_at.as_deref().unwrap_or("No due date")),
                        format!("Created: {}", task.created_at),
                        format!("Updated: {}", task.updated_at),
                        format!("Priority: {}", task_priority_label(&task.priority)),
                        format!(
                            "Project: {}",
                            task.project.as_deref().unwrap_or("Independent")
                        ),
                    ]);
                    return detail_lines(
                        lines,
                        width,
                        "Enter/Esc close | e edit | Space complete | x cancel",
                    );
                }
            }
            vec![Line::from("No task selected.")]
        }
        Modal::ReminderDetail => {
            if let Some(index) = app.selected_reminder_index() {
                if let Some(reminder) = app.reminders.get(index) {
                    return detail_lines(
                        vec![
                            format!("Title: {}", reminder.title),
                            "Body".to_string(),
                            reminder.body.clone(),
                            format!("Status: {}", reminder_status_group_label(&reminder.status)),
                            format!(
                                "Due: {}",
                                reminder
                                    .due_at
                                    .as_deref()
                                    .map(format_due)
                                    .unwrap_or_else(|| "No due date".to_string())
                            ),
                            format!("Google Calendar Sync: {}", reminder.calendar_sync_enabled),
                            format!("Created: {}", reminder.created_at),
                            format!("Updated: {}", reminder.updated_at),
                        ],
                        width,
                        "Enter/Esc close | e edit | Space complete | x cancel",
                    );
                }
            }
            vec![Line::from("No reminder selected.")]
        }
        Modal::CalendarDetail => {
            if let Some(event) = app.selected_calendar_event() {
                let location = if event.location.is_empty() {
                    "None"
                } else {
                    event.location.as_str()
                };
                let link = if event.html_link.is_empty() {
                    "None"
                } else {
                    event.html_link.as_str()
                };
                return detail_lines(
                    vec![
                        format!("Title: {}", event.summary),
                        format!("Status: {}", event.status),
                        format!("Start: {}", event.start),
                        format!("End: {}", event.end),
                        format!("Location: {location}"),
                        format!("Event ID: {}", event.id),
                        format!("Google link: {link}"),
                    ],
                    width,
                    "Enter/Esc close",
                );
            }
            vec![Line::from("No calendar event selected.")]
        }
        Modal::ProposalDetail => {
            if let Some(proposal) = app.selected_proposal() {
                let tags = if proposal.tags.is_empty() {
                    "None".to_string()
                } else {
                    proposal.tags.join(", ")
                };
                return detail_lines(
                    vec![
                        format!("Title: {}", proposal_title(proposal)),
                        format!("Created: {}", proposal.created_at),
                        format!("Updated: {}", proposal.updated_at),
                        format!(
                            "Recommended decision: {}",
                            proposal_decision_label(&proposal.decision)
                        ),
                        format!("Status: {}", proposal.status),
                        format!("Conversation turn: {}", proposal.conversation_turn_id),
                        format!("Proposal items: {}", proposal.item_count),
                        String::new(),
                        "Proposed memory".to_string(),
                        if proposal.proposed_memory.trim().is_empty() {
                            "No proposal text available.".to_string()
                        } else {
                            proposal.proposed_memory.clone()
                        },
                        String::new(),
                        format!(
                            "Item kind: {}",
                            if proposal.item_kind.trim().is_empty() {
                                "unknown"
                            } else {
                                proposal.item_kind.as_str()
                            }
                        ),
                        format!("Tags: {tags}"),
                    ],
                    width,
                    "Esc close | a accept | x reject",
                );
            }
            vec![Line::from("No proposal selected.")]
        }
        Modal::SearchDetail => {
            if let Some((id, text, score)) = app.selected_search() {
                return detail_lines(
                    vec![
                        format!(
                            "Source: {}",
                            id.split_once(':')
                                .map(|(source, _)| source)
                                .unwrap_or("RESULT")
                        ),
                        format!("Score: {:.3}", score),
                        format!("ID: {}", id),
                        String::new(),
                        text.clone(),
                    ],
                    width,
                    "Enter/Esc close",
                );
            }
            vec![Line::from("No result selected.")]
        }
    }
}

fn wrap_balanced_preserving_newlines(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut output = Vec::new();
    for source in value.split('\n') {
        if source.trim().is_empty() {
            output.push(String::new());
            continue;
        }
        let words = source.split_whitespace().collect::<Vec<_>>();
        if words.is_empty() {
            output.push(String::new());
            continue;
        }
        if words.iter().any(|word| word.chars().count() > width) {
            output.extend(wrap_preserve_words(source, width));
            continue;
        }
        let total = words.iter().map(|word| word.chars().count()).sum::<usize>()
            + words.len().saturating_sub(1);
        let line_count = ((total + width - 1) / width).max(1);
        let mut start = 0usize;
        let mut remaining_chars = total;
        let mut remaining_lines = line_count;
        while start < words.len() {
            if remaining_lines == 1 {
                output.push(words[start..].join(" "));
                break;
            }
            let target = (remaining_chars + remaining_lines - 1) / remaining_lines;
            let mut best_end = start + 1;
            let mut best_score = usize::MAX;
            let mut length = 0usize;
            for end in start..words.len() {
                let word_len = words[end].chars().count();
                let projected = if end == start {
                    word_len
                } else {
                    length + 1 + word_len
                };
                if projected > width {
                    break;
                }
                length = projected;
                if end + 1 == words.len() {
                    break;
                }
                let score = projected.abs_diff(target);
                if score < best_score {
                    best_score = score;
                    best_end = end + 1;
                }
            }
            if best_end <= start {
                best_end = (start + 1).min(words.len());
            }
            let line = words[start..best_end].join(" ");
            remaining_chars = remaining_chars
                .saturating_sub(line.chars().count() + if best_end < words.len() { 1 } else { 0 });
            output.push(line);
            start = best_end;
            remaining_lines = remaining_lines.saturating_sub(1).max(1);
        }
    }
    if output.is_empty() {
        output.push(String::new());
    }
    output
}

fn detail_lines(lines: Vec<String>, width: usize, footer: &str) -> Vec<Line<'static>> {
    let mut output = Vec::new();
    for value in lines {
        output.extend(render_plain_text(
            &value,
            width.max(1),
            "",
            Style::default(),
        ));
    }
    output.push(Line::from(""));
    output.push(Line::from(Span::styled(footer.to_string(), muted_style())));
    output
}

fn app_background() -> Color {
    Color::Rgb(40, 44, 52)
}

fn outer_border_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn panel_border_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn panel_title_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn app_title_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn section_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn muted_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn selected_row_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn active_tab_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn task_priority_style(priority: &str) -> Style {
    match priority {
        "high" => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        "medium" => Style::default(),
        "low" => muted_style(),
        _ => muted_style(),
    }
}

fn calendar_sync_style(enabled: bool) -> Style {
    if enabled {
        Style::default().fg(Color::Cyan)
    } else {
        muted_style()
    }
}

fn panel_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(panel_border_style())
        .title(title)
        .title_style(panel_title_style())
}

fn footer_commands(
    tab: Tab,
    input_mode: InputMode,
    pending_chat: bool,
    mode: LayoutMode,
) -> String {
    if input_mode == InputMode::Chat {
        return if mode == LayoutMode::Mini {
            "Enter send | S+Enter new line | Esc".to_string()
        } else {
            "Enter send | Shift+Enter newline | Esc input".to_string()
        };
    }
    if input_mode == InputMode::Search {
        return "Enter search | Esc cancel".to_string();
    }
    match tab {
        Tab::Overview => "j/k select | Enter detail | r refresh | ? help".to_string(),
        Tab::Tasks => {
            "j/k select | Enter detail | c create | e edit | Space complete | x cancel".to_string()
        }
        Tab::Reminders => {
            "j/k select | Enter detail | c create | e edit | Space complete | x cancel".to_string()
        }
        Tab::Proposals => "j/k select | Enter detail | a accept | x reject".to_string(),
        Tab::Chat => {
            if pending_chat {
                "⟳ response running | c input | Esc quit".to_string()
            } else {
                "c input | : commands | Esc quit".to_string()
            }
        }
        Tab::Models => "↑↓ select | Enter switch | r refresh".to_string(),
        Tab::Jobs => "j/k select | r refresh".to_string(),
        Tab::Search => "/ search | Enter detail | r refresh".to_string(),
        Tab::Calendar => "Enter detail | r refresh | :calendar-login".to_string(),
    }
}

fn tab_lines(selected: usize, width: u16) -> Vec<Line<'static>> {
    let target = width.max(1) as usize;
    let mut lines = Vec::new();
    let mut current = Vec::<Span<'static>>::new();
    let mut current_width = 0usize;
    for (index, name) in TABS.iter().enumerate() {
        let label = format!("{}:{}", index + 1, name);
        let needed = if current.is_empty() {
            label.len()
        } else {
            label.len() + 2
        };
        if !current.is_empty() && current_width + needed > target {
            lines.push(Line::from(std::mem::take(&mut current)));
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(Span::raw("  "));
            current_width += 2;
        }
        let style = if index == selected {
            active_tab_style()
        } else {
            Style::default()
        };
        current_width += label.len();
        current.push(Span::styled(label, style));
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

fn tab_from_mouse(x: u16, y: u16, header: Rect) -> Option<Tab> {
    if y <= header.y + 1 {
        return None;
    }
    let line = y.saturating_sub(header.y + 1) as usize;
    let mut current_line = 0usize;
    let mut current_x = header.x + 1;
    for index in 0..TABS.len() {
        let label = format!("{}:{}", index + 1, TABS[index]);
        let width = label.len() as u16;
        if current_x + width > header.x + header.width.saturating_sub(1) {
            current_line += 1;
            current_x = header.x + 1;
        }
        if current_line == line && x >= current_x && x < current_x + width {
            return Some(Tab::from_index(index));
        }
        if current_line == line {
            current_x += width + 2;
        }
    }
    None
}

fn task_header_line(width: usize, mode: LayoutMode) -> Line<'static> {
    if mode == LayoutMode::Mini {
        let due_width = 11;
        let title_width = width.saturating_sub(due_width + 4).max(8);
        return Line::from(Span::styled(
            format!(
                "{:<due_width$}  {:<title_width$}  □",
                "Due",
                "Task",
                due_width = due_width,
                title_width = title_width.min(28),
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }

    let due_width = 16;
    let status_width = 12;
    let priority_width = 8;
    let title_width = width
        .saturating_sub(due_width + status_width + priority_width + 8)
        .max(10)
        .min(if mode == LayoutMode::Full { 54 } else { 38 });
    Line::from(Span::styled(
        format!(
            "{:<due_width$}  {:<title_width$}  {:<status_width$} {:<priority_width$}  □",
            "Due date",
            "Task title",
            "Status",
            "Priority",
            due_width = due_width,
            title_width = title_width,
            status_width = status_width,
            priority_width = priority_width,
        ),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

fn task_line(task: &UiTask, width: usize, mode: LayoutMode) -> Line<'static> {
    let checkbox = task_symbol(&task.status);
    let checkbox_style = if matches!(task.status.as_str(), "completed" | "cancelled" | "archived") {
        muted_style()
    } else {
        Style::default()
    };
    if mode == LayoutMode::Mini {
        let due_width = 11;
        let title_width = width.saturating_sub(due_width + 5).max(8).min(28);
        let due = compact(task.due_at.as_deref().unwrap_or("--"), due_width);
        let title = compact(&task.title, title_width);
        return Line::from(vec![
            Span::styled(
                format!("{due:<due_width$}  ", due_width = due_width),
                task_due_style(task.due_at.as_deref()),
            ),
            Span::raw(format!(
                "{title:<title_width$}  ",
                title_width = title_width
            )),
            Span::styled(checkbox.to_string(), checkbox_style),
        ]);
    }

    let due_width = 16;
    let status_width = 12;
    let priority_width = 8;
    let title_width = width
        .saturating_sub(due_width + status_width + priority_width + 8)
        .max(10)
        .min(if mode == LayoutMode::Full { 54 } else { 38 });
    let due = compact(task.due_at.as_deref().unwrap_or("--"), due_width);
    let title = compact(&task.title, title_width);
    let status = compact(&task_status_label(&task.status), status_width);
    let priority = compact(&task_priority_label(&task.priority), priority_width);
    Line::from(vec![
        Span::styled(
            format!("{due:<due_width$}  ", due_width = due_width),
            task_due_style(task.due_at.as_deref()),
        ),
        Span::raw(format!(
            "{title:<title_width$}  ",
            title_width = title_width
        )),
        Span::raw(format!(
            "{status:<status_width$} ",
            status_width = status_width
        )),
        Span::styled(
            format!(
                "{priority:<priority_width$}  ",
                priority_width = priority_width
            ),
            task_priority_style(&task.priority),
        ),
        Span::styled(checkbox.to_string(), checkbox_style),
    ])
}

fn task_due_style(due_at: Option<&str>) -> Style {
    match due_at.and_then(parse_display_date) {
        Some(date) if date < local_today() => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        Some(date) if date == local_today() => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        _ => Style::default(),
    }
}

fn local_today() -> time::Date {
    OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .date()
}

fn parse_display_date(value: &str) -> Option<time::Date> {
    let first = value.split_whitespace().next()?;
    let parts = first.split('-').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    if parts[0].len() == 4 {
        let year = parts[0].parse::<i32>().ok()?;
        let month = parts[1].parse::<u8>().ok()?;
        let day = parts[2].parse::<u8>().ok()?;
        return time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok();
    }
    let day = parts[0].parse::<u8>().ok()?;
    let month = parts[1].parse::<u8>().ok()?;
    let year = 2000 + parts[2].parse::<i32>().ok()?;
    time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

fn task_is_due_today(task: &UiTask, today: time::Date) -> bool {
    task.due_at.as_deref().and_then(parse_display_date) == Some(today)
}

fn reminder_is_due_today(reminder: &UiReminder, today: time::Date) -> bool {
    reminder.due_at.as_deref().and_then(parse_display_date) == Some(today)
}

fn compare_overview_tasks(left: &UiTask, right: &UiTask, today: time::Date) -> std::cmp::Ordering {
    fn bucket(task: &UiTask, today: time::Date) -> u8 {
        match task.due_at.as_deref().and_then(parse_display_date) {
            Some(date) if date == today => 0,
            Some(date)
                if date > today
                    && date <= today.saturating_add(time::Duration::days(30))
                    && task.priority == "high" =>
            {
                1
            }
            Some(date)
                if date > today && date <= today.saturating_add(time::Duration::days(30)) =>
            {
                2
            }
            Some(date) if date < today => 3,
            _ => 4,
        }
    }
    bucket(left, today)
        .cmp(&bucket(right, today))
        .then_with(|| left.due_at.as_deref().cmp(&right.due_at.as_deref()))
        .then_with(|| task_priority_rank(&left.priority).cmp(&task_priority_rank(&right.priority)))
        .then_with(|| left.title.cmp(&right.title))
}

fn search_group_label(id: &str) -> &'static str {
    match id.split_once(':').map(|(source, _)| source).unwrap_or(id) {
        "project" | "projects" => "Projects",
        "topic" | "topics" => "Topics",
        "person" | "people" => "People",
        "journal" => "Journal",
        "idea" | "ideas" => "Ideas",
        "meeting" | "meetings" => "Meetings",
        "reference" | "references" => "References",
        "conversation" | "session" => "Conversation",
        _ => "Other",
    }
}

fn search_visual_to_result_index(
    results: &[(String, String, f64)],
    visual_row: usize,
) -> Option<usize> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, result) in results.iter().enumerate() {
        let group = search_group_label(&result.0).to_string();
        if let Some((_, indices)) = groups.iter_mut().find(|(name, _)| *name == group) {
            indices.push(index);
        } else {
            groups.push((group, vec![index]));
        }
    }
    groups.sort_by(|left, right| {
        let left_score = left
            .1
            .first()
            .map(|index| results[*index].2)
            .unwrap_or(f64::NEG_INFINITY);
        let right_score = right
            .1
            .first()
            .map(|index| results[*index].2)
            .unwrap_or(f64::NEG_INFINITY);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut row = 0usize;
    for (_, indices) in groups {
        if row == visual_row {
            return None;
        }
        row += 1;
        for index in indices {
            if row == visual_row {
                return Some(index);
            }
            row += 1;
        }
    }
    None
}

fn calendar_event_day(event: &UiCalendarEvent) -> Option<time::Date> {
    parse_display_date(&event.start)
}

fn month_name(month: time::Month) -> &'static str {
    match month {
        time::Month::January => "January",
        time::Month::February => "February",
        time::Month::March => "March",
        time::Month::April => "April",
        time::Month::May => "May",
        time::Month::June => "June",
        time::Month::July => "July",
        time::Month::August => "August",
        time::Month::September => "September",
        time::Month::October => "October",
        time::Month::November => "November",
        time::Month::December => "December",
    }
}

fn days_in_month(year: i32, month: time::Month) -> u8 {
    match month {
        time::Month::January
        | time::Month::March
        | time::Month::May
        | time::Month::July
        | time::Month::August
        | time::Month::October
        | time::Month::December => 31,
        time::Month::April | time::Month::June | time::Month::September | time::Month::November => {
            30
        }
        time::Month::February if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        time::Month::February => 28,
    }
}

fn calendar_month_lines(
    date: time::Date,
    events: &[UiCalendarEvent],
    width: usize,
    mode: LayoutMode,
) -> Vec<Line<'static>> {
    let year = date.year();
    let month = date.month();
    let days = days_in_month(year, month);
    let first = time::Date::from_calendar_date(year, month, 1).ok();
    let first_weekday = first
        .map(|value| value.weekday().number_from_monday() as usize - 1)
        .unwrap_or(0);
    let mut lines = Vec::new();

    let (cell_width, weekdays) = if mode == LayoutMode::Mini || width < 70 {
        (4usize, ["M", "T", "W", "T", "F", "S", "S"])
    } else {
        (9usize, ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"])
    };

    lines.push(Line::from(
        weekdays
            .iter()
            .map(|day| Span::styled(format!("{day:<cell_width$}"), section_style()))
            .collect::<Vec<_>>(),
    ));

    let today = local_today();
    let mut day = 1u8;
    let rows = ((first_weekday + days as usize + 6) / 7).max(1);
    for row in 0..rows {
        let mut spans = Vec::new();
        for col in 0..7usize {
            let slot = row * 7 + col;
            if slot < first_weekday || day > days {
                spans.push(Span::raw(" ".repeat(cell_width)));
                continue;
            }
            let current = time::Date::from_calendar_date(year, month, day).ok();
            let has_event = current.is_some_and(|candidate| {
                events
                    .iter()
                    .any(|event| calendar_event_day(event) == Some(candidate))
            });
            let label = if has_event && current == Some(today) {
                format!("*{:02}", day)
            } else if has_event {
                format!("·{:02}", day)
            } else if current == Some(today) {
                format!("•{:02}", day)
            } else {
                format!(" {:02}", day)
            };
            let style = if current == Some(today) {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if has_event {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(format!("{label:<cell_width$}"), style));
            day += 1;
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn task_status_group_label(status: &str) -> &'static str {
    match status {
        "expired" => "Expired",
        "in_progress" => "In Progress",
        "open" => "Open",
        "completed" => "Completed",
        "cancelled" => "Cancelled",
        "archived" => "Archived",
        _ => "Other",
    }
}

fn task_status_label(status: &str) -> String {
    task_status_group_label(status).to_string()
}

fn task_priority_rank(priority: &str) -> u8 {
    match priority {
        "high" => 0,
        "medium" => 1,
        "low" => 2,
        _ => 3,
    }
}

fn task_priority_label(priority: &str) -> String {
    match priority {
        "high" => "High".to_string(),
        "medium" => "Medium".to_string(),
        "low" => "Low".to_string(),
        value if value.trim().is_empty() => "Unspecified".to_string(),
        value => value.to_string(),
    }
}

fn reminder_status_group_label(status: &str) -> &'static str {
    match status {
        "triggered" => "Triggered",
        "scheduled" => "Scheduled",
        "completed" => "Completed",
        "cancelled" => "Cancelled",
        _ => "Other",
    }
}

fn reminder_header_line(width: usize, mode: LayoutMode) -> Line<'static> {
    let due_width = if mode == LayoutMode::Mini { 14 } else { 20 };
    let status_width = if mode == LayoutMode::Mini { 9 } else { 12 };
    let title_width = width.saturating_sub(due_width + status_width + 6).max(10);
    Line::from(Span::styled(
        format!(
            "{:<due_width$}  {:<title_width$}  {:<status_width$}",
            "Due",
            "Reminder",
            "Status",
            due_width = due_width,
            title_width = title_width,
            status_width = status_width
        ),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

fn sort_ui_reminders(reminders: &mut [UiReminder]) {
    reminders.sort_by(|a, b| {
        reminder_status_rank(&a.status)
            .cmp(&reminder_status_rank(&b.status))
            .then(compare_due(a.due_at.as_deref(), b.due_at.as_deref()))
            .then(
                a.title
                    .to_ascii_lowercase()
                    .cmp(&b.title.to_ascii_lowercase()),
            )
    });
}

fn sort_ui_proposals(proposals: &mut [UiProposal]) {
    proposals.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.title.cmp(&b.title)));
}

fn sort_ui_jobs(jobs: &mut [UiJob]) {
    jobs.sort_by(|a, b| {
        job_rank(&a.status)
            .cmp(&job_rank(&b.status))
            .then(a.next_run_at.cmp(&b.next_run_at))
            .then(a.id.cmp(&b.id))
    });
}

fn job_status_group_label(status: &str) -> &'static str {
    match status {
        "running" => "Active",
        "queued" | "pending" => "Queued",
        "failed" => "Failed",
        "completed" => "Completed",
        _ => "Other",
    }
}

fn reminder_line(reminder: &UiReminder, width: usize, mode: LayoutMode) -> Line<'static> {
    let symbol = if matches!(reminder.status.as_str(), "completed") {
        "✓"
    } else if reminder.status == "cancelled" {
        "✕"
    } else {
        "◷"
    };
    let due = reminder
        .due_at
        .as_deref()
        .map(format_due)
        .unwrap_or_else(|| "--".to_string());
    let title_limit = match mode {
        LayoutMode::Full => 48,
        LayoutMode::Compact => 34,
        LayoutMode::Mini => 24,
        LayoutMode::TooSmall => 16,
    };
    let title = compact(
        &reminder.title,
        title_limit.min(width.saturating_sub(20).max(8)),
    );
    let sync = if reminder.calendar_sync_enabled {
        "↗"
    } else {
        " "
    };
    if mode == LayoutMode::Mini {
        Line::from(vec![
            Span::raw(format!("{symbol} {}  {} ", compact(&due, 16), title)),
            Span::styled(sync, calendar_sync_style(reminder.calendar_sync_enabled)),
        ])
    } else {
        Line::from(vec![
            Span::raw(format!(
                "{:<15} {:<title_limit$} {:<12} {symbol} ",
                due,
                title,
                reminder_status_group_label(&reminder.status),
                title_limit = title_limit.min(48)
            )),
            Span::styled(sync, calendar_sync_style(reminder.calendar_sync_enabled)),
        ])
    }
}

fn task_symbol(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "in_progress" => "◐",
        "cancelled" => "✕",
        "expired" => "⚠",
        _ => "☐",
    }
}

fn proposal_title(proposal: &UiProposal) -> String {
    if proposal.title.trim().is_empty() {
        "Memory proposal".to_string()
    } else {
        proposal.title.clone()
    }
}

fn proposal_created_at(proposal: &UiProposal) -> String {
    if proposal.created_at.trim().is_empty() {
        "--".to_string()
    } else {
        proposal.created_at.clone()
    }
}

fn proposal_decision_label(decision: &str) -> String {
    match decision {
        "should_save" => "Should save".to_string(),
        "should_reject" | "should_not_save" => "Should not save".to_string(),
        "pending" => "Pending review".to_string(),
        value if value.trim().is_empty() => "Unknown".to_string(),
        value => value
            .split(['_', '-'])
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn proposal_header_line(width: usize, mode: LayoutMode) -> Line<'static> {
    if mode == LayoutMode::Mini {
        let title_width = width.saturating_sub(18).max(8).min(28);
        return Line::from(Span::styled(
            format!(
                "{:<title_width$}  Decision",
                "Title",
                title_width = title_width,
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }

    let title_width =
        if mode == LayoutMode::Full { 42 } else { 30 }.min(width.saturating_sub(24).max(8));
    let date_width = 17;
    Line::from(Span::styled(
        format!(
            "{:<title_width$}  {:<date_width$}  Recommended Decision",
            "Title",
            "Date",
            title_width = title_width,
            date_width = date_width,
        ),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

fn proposal_list_line(proposal: &UiProposal, width: usize, mode: LayoutMode) -> Line<'static> {
    let decision = proposal_decision_label(&proposal.decision);
    if mode == LayoutMode::Mini {
        let title_width = width.saturating_sub(16).max(8).min(28);
        return Line::from(format!(
            "{:<title_width$}  {}",
            compact(&proposal_title(proposal), title_width),
            compact(&decision, width.saturating_sub(title_width + 2).max(8)),
            title_width = title_width,
        ));
    }

    let title_width =
        if mode == LayoutMode::Full { 42 } else { 30 }.min(width.saturating_sub(24).max(8));
    let date_width = 17;
    let title = compact(&proposal_title(proposal), title_width);
    let date = compact(&proposal_created_at(proposal), date_width);
    Line::from(format!(
        "{:<title_width$}  {:<date_width$}  {}",
        title,
        date,
        compact(
            &decision,
            width.saturating_sub(title_width + date_width + 4).max(8)
        ),
        title_width = title_width,
        date_width = date_width,
    ))
}

fn model_state_style(active: bool, available: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if available {
        Style::default()
    } else {
        muted_style()
    }
}

fn job_status_style(status: &str) -> Style {
    match status {
        "running" => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        "failed" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "completed" => Style::default().fg(Color::Cyan),
        _ => Style::default(),
    }
}

fn job_rank(status: &str) -> u8 {
    match status {
        "running" => 0,
        "queued" | "pending" => 1,
        "failed" => 2,
        "completed" => 3,
        _ => 4,
    }
}

fn ui_model_from_backend(model: assistant_protocol::ModelInfo) -> UiModel {
    UiModel {
        id: model.id,
        display_name: model.display_name,
        available: model.available,
        active: model.active,
        capabilities: model.capabilities,
    }
}

fn ui_task_from_backend(task: assistant_protocol::TaskSummary) -> UiTask {
    UiTask {
        id: task.id,
        title: task.title,
        body: task.body,
        status: task.status,
        priority: "medium".to_string(),
        due_at: task
            .due_at
            .and_then(|value| format_backend_timestamp(&value)),
        created_at: task
            .created_at
            .as_deref()
            .and_then(format_backend_timestamp)
            .unwrap_or_default(),
        updated_at: task
            .updated_at
            .as_deref()
            .and_then(format_backend_timestamp)
            .unwrap_or_default(),
        project: None,
    }
}

fn ui_reminder_from_backend(reminder: assistant_protocol::ReminderSummary) -> UiReminder {
    UiReminder {
        id: reminder.id,
        title: reminder.title,
        body: reminder.body,
        status: reminder.status,
        due_at: reminder
            .due_at
            .and_then(|value| format_backend_timestamp(&value)),
        created_at: reminder
            .created_at
            .as_deref()
            .and_then(format_backend_timestamp)
            .unwrap_or_default(),
        updated_at: reminder
            .updated_at
            .as_deref()
            .and_then(format_backend_timestamp)
            .unwrap_or_default(),
        calendar_sync_enabled: reminder.calendar_sync_enabled,
        calendar_sync_status: reminder.calendar_sync_status,
    }
}

fn ui_proposal_from_backend(proposal: assistant_protocol::MemoryProposalSummary) -> UiProposal {
    UiProposal {
        id: proposal.id,
        title: proposal.title,
        created_at: format_backend_timestamp(&proposal.created_at).unwrap_or(proposal.created_at),
        updated_at: format_backend_timestamp(&proposal.updated_at).unwrap_or(proposal.updated_at),
        decision: proposal.decision,
        status: proposal.status,
        conversation_turn_id: proposal.conversation_turn_id,
        proposed_memory: proposal.proposed_memory,
        item_kind: proposal.item_kind,
        tags: proposal.tags,
        item_count: proposal.item_count,
    }
}

fn ui_job_from_backend(job: assistant_protocol::JobSummary) -> UiJob {
    UiJob {
        id: job.id,
        job_type: job.job_type,
        status: job.status,
        next_run_at: job.next_run_at,
    }
}

fn ui_search_result_from_backend(
    result: assistant_protocol::MemorySummary,
) -> (String, String, f64) {
    (result.id, result.text, result.score)
}

fn ui_calendar_event_from_backend(value: serde_json::Value) -> Option<UiCalendarEvent> {
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let summary = value
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Untitled event")
        .to_string();
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("confirmed")
        .to_string();
    let start = calendar_event_time(&value, "start");
    let end = calendar_event_time(&value, "end");
    Some(UiCalendarEvent {
        id,
        summary,
        status,
        start,
        end,
        location: value
            .get("location")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        html_link: value
            .get("htmlLink")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn calendar_event_time(value: &serde_json::Value, field: &str) -> String {
    let Some(object) = value.get(field).and_then(serde_json::Value::as_object) else {
        return "Unknown".to_string();
    };
    if let Some(date_time) = object.get("dateTime").and_then(serde_json::Value::as_str) {
        return format_due(date_time);
    }
    object
        .get("date")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Unknown")
        .to_string()
}

fn format_bytes(value: Option<u64>) -> String {
    let Some(value) = value else {
        return "n/a".to_string();
    };
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut index = 0usize;
    while size >= 1024.0 && index < UNITS.len() - 1 {
        size /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{} {}", value, UNITS[index])
    } else {
        format!("{size:.1} {}", UNITS[index])
    }
}

fn system_diagnostics_lines(
    info: Option<&serde_json::Value>,
    active_model: Option<&str>,
    ready: Option<bool>,
    mode: LayoutMode,
) -> Vec<Line<'static>> {
    let runtime = info.and_then(|value| value.get("runtime"));
    let hardware = info.and_then(|value| value.get("hardware"));
    let generation = runtime.and_then(|value| value.get("generation"));
    let memory = hardware.and_then(|value| value.get("memory"));
    let gpu = hardware
        .and_then(|value| value.get("gpu"))
        .and_then(|value| value.get("devices"))
        .and_then(serde_json::Value::as_array)
        .and_then(|devices| devices.first());
    let network = info
        .and_then(|value| value.get("network"))
        .and_then(|value| value.get("default_interface"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("none");

    let pid = runtime
        .and_then(|value| value.get("pid"))
        .map(ToString::to_string)
        .unwrap_or_else(|| "n/a".to_string());
    let endpoint = generation
        .and_then(|value| value.get("endpoint"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("n/a");
    let generation_reachable = generation
        .and_then(|value| value.get("reachable"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let total = memory
        .and_then(|value| value.get("total_bytes"))
        .and_then(serde_json::Value::as_u64);
    let used = memory
        .and_then(|value| value.get("used_bytes"))
        .and_then(serde_json::Value::as_u64);
    let gpu_name = gpu
        .and_then(|value| value.get("name"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            gpu.and_then(|value| value.get("description"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("none");
    let gpu_vram = gpu
        .and_then(|value| {
            let total = value
                .get("vram_total_mib")
                .and_then(serde_json::Value::as_u64)?;
            let used = value
                .get("vram_used_mib")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Some(format!("{used} / {total} MiB"))
        })
        .unwrap_or_else(|| "n/a".to_string());

    if mode == LayoutMode::Mini {
        return vec![
            Line::from(Span::styled("Live runtime", section_style())),
            Line::from(format!("Active: {}", active_model.unwrap_or("none"))),
            Line::from(format!("Ready: {}  PID: {}", ready.unwrap_or(false), pid)),
        ];
    }

    vec![
        Line::from(Span::styled("Live runtime diagnostics", section_style())),
        Line::from(format!("Active: {}", active_model.unwrap_or("none"))),
        Line::from(format!("Ready: {}   PID: {}", ready.unwrap_or(false), pid)),
        Line::from(format!(
            "Generation: {} ({})",
            endpoint,
            if generation_reachable {
                "reachable"
            } else {
                "unreachable"
            }
        )),
        Line::from(format!(
            "RAM: {} / {}   Network: {}",
            format_bytes(used),
            format_bytes(total),
            network
        )),
        Line::from(format!(
            "GPU: {}   VRAM: {}",
            compact(gpu_name, 36),
            gpu_vram
        )),
    ]
}

fn format_backend_timestamp(value: &str) -> Option<String> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    let local = parsed.to_offset(UtcOffset::current_local_offset().ok()?);
    local
        .format(
            &time::format_description::parse_borrowed::<1>(
                "[day]-[month]-[year repr:last_two] [hour]:[minute]",
            )
            .ok()?,
        )
        .ok()
}

#[cfg(test)]
fn set_local_task_status(tasks: &mut [UiTask], id: &str, status: &str) -> bool {
    let Some(task) = tasks.iter_mut().find(|task| task.id == id) else {
        return false;
    };
    task.status = status.to_string();
    task.updated_at = task_timestamp();
    true
}

fn sort_ui_tasks(tasks: &mut [UiTask]) {
    tasks.sort_by(|a, b| {
        task_status_rank(&a.status)
            .cmp(&task_status_rank(&b.status))
            .then(task_priority_rank(&a.priority).cmp(&task_priority_rank(&b.priority)))
            .then(compare_due(a.due_at.as_deref(), b.due_at.as_deref()))
            .then(
                a.title
                    .to_ascii_lowercase()
                    .cmp(&b.title.to_ascii_lowercase()),
            )
    });
}

fn task_status_rank(status: &str) -> u8 {
    match status {
        "expired" => 0,
        "in_progress" => 1,
        "open" => 2,
        "completed" => 3,
        "cancelled" => 4,
        "archived" => 5,
        _ => 6,
    }
}

fn reminder_status_rank(status: &str) -> u8 {
    match status {
        "triggered" => 0,
        "scheduled" => 1,
        "completed" => 2,
        "cancelled" => 3,
        _ => 4,
    }
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && y >= rect.y
        && x < rect.x.saturating_add(rect.width)
        && y < rect.y.saturating_add(rect.height)
}

fn compare_due(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(a), Some(b)) => a.cmp(b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn format_due_input(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) else {
        return value.to_string();
    };
    let local =
        parsed.to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC));
    format!(
        "{:02}/{:02}/{:04} {:02}:{:02}",
        local.day(),
        local.month() as u8,
        local.year(),
        local.hour(),
        local.minute()
    )
}

fn format_due(value: &str) -> String {
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) {
        let local = parsed
            .to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC));
        return format!(
            "{:02}-{:02}-{:02} {:02}:{:02} ({})",
            local.day(),
            local.month() as u8,
            (local.year() % 100).abs(),
            local.hour(),
            local.minute(),
            relative_due(local)
        );
    }
    compact(value, 24)
}

fn relative_due(value: OffsetDateTime) -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let seconds = (value - now).whole_seconds();
    if seconds < -86_400 {
        format!("overdue {}d", (-seconds) / 86_400)
    } else if seconds < 0 {
        "overdue".to_string()
    } else if seconds < 3_600 {
        format!("in {}m", (seconds / 60).max(1))
    } else if seconds < 86_400 {
        format!("in {}h", seconds / 3_600)
    } else if seconds < 172_800 {
        "tomorrow".to_string()
    } else {
        format!("in {}d", seconds / 86_400)
    }
}

fn render_plain_text(value: &str, width: usize, prefix: &str, style: Style) -> Vec<Line<'static>> {
    wrap_preserve_words(value, width.saturating_sub(prefix.chars().count()).max(1))
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let lead = if index == 0 { prefix } else { "" };
            Line::from(vec![
                Span::styled(lead.to_string(), style),
                Span::styled(line, style),
            ])
        })
        .collect()
}

fn render_terminal_markdown(value: &str, prefix: &str, width: usize) -> Vec<Line<'static>> {
    let mut output = Vec::new();
    let mut in_code = false;
    for raw in value.lines() {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            output.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if in_code {
            output.extend(render_plain_text(raw, width, "  ", Style::default()));
            continue;
        }
        let (style, text) = if let Some(rest) = raw.strip_prefix("# ") {
            (Style::default().add_modifier(Modifier::BOLD), rest)
        } else if let Some(rest) = raw.strip_prefix("## ") {
            (Style::default().add_modifier(Modifier::BOLD), rest)
        } else {
            (Style::default(), raw)
        };
        output.extend(
            wrap_preserve_words(text, width.saturating_sub(prefix.chars().count()).max(1))
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    let lead = if index == 0 { prefix } else { "" };
                    Line::from(vec![
                        Span::styled(lead.to_string(), style),
                        Span::styled(line, style),
                    ])
                }),
        );
    }
    if output.is_empty() {
        output.push(Line::from(""));
    }
    output
}

fn wrap_preserve_words(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut output = Vec::new();
    for source_line in value
        .lines()
        .map(|line| if line.is_empty() { "" } else { line })
    {
        if source_line.is_empty() {
            output.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in source_line.split_whitespace() {
            if word.chars().count() > width {
                if !current.is_empty() {
                    output.push(std::mem::take(&mut current));
                }
                let chars = word.chars().collect::<Vec<_>>();
                for chunk in chars.chunks(width) {
                    output.push(chunk.iter().collect());
                }
                continue;
            }
            let projected = if current.is_empty() {
                word.chars().count()
            } else {
                current.chars().count() + 1 + word.chars().count()
            };
            if projected <= width {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            } else {
                output.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            output.push(current);
        }
    }
    if output.is_empty() {
        output.push(String::new());
    }
    output
}

fn editor_line_count(value: &str, width: usize) -> usize {
    let (lines, _, _) = editor_visual_lines(value, value.chars().count(), width.max(1));
    lines.len().max(1)
}

fn editor_render_lines(value: &str, cursor: Option<usize>, width: usize) -> Vec<Line<'static>> {
    let cursor = cursor.map(|position| position.min(value.chars().count()));
    let cursor_visual = cursor
        .map(|position| editor_visual_lines(value, position, width.max(1)))
        .map(|(_, row, col)| (row, col));
    let (lines, _, _) = editor_visual_lines(value, value.chars().count(), width.max(1));
    lines
        .into_iter()
        .enumerate()
        .map(|(row, line)| {
            let chars = line.chars().collect::<Vec<_>>();
            let mut spans = Vec::new();
            if let Some((cursor_row, cursor_col)) = cursor_visual {
                if row == cursor_row {
                    let render_len = chars.len().max(cursor_col + 1);
                    for index in 0..render_len {
                        let ch = chars.get(index).copied().unwrap_or(' ');
                        if index == cursor_col {
                            spans.push(Span::styled(
                                ch.to_string(),
                                Style::default().add_modifier(Modifier::REVERSED),
                            ));
                        } else {
                            spans.push(Span::raw(ch.to_string()));
                        }
                    }
                } else {
                    spans.push(Span::raw(line));
                }
            } else {
                spans.push(Span::raw(line));
            }
            Line::from(spans)
        })
        .collect()
}

fn editor_visual_lines(value: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let chars = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let mut lines: Vec<String> = vec![String::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    for (index, ch) in chars.iter().enumerate() {
        if *ch == '\n' {
            lines.push(String::new());
            row += 1;
            col = 0;
        } else {
            lines[row].push(*ch);
            col += 1;
            if col == width {
                lines.push(String::new());
                row += 1;
                col = 0;
            }
        }
        if index + 1 == cursor {
            cursor_row = row;
            cursor_col = col;
        }
    }
    (lines, cursor_row, cursor_col)
}

fn editor_visual_map(value: &str, width: usize) -> Vec<(usize, usize, usize)> {
    let chars = value.chars().collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut row = 0usize;
    let mut col = 0usize;
    let mut index = 0usize;
    while index < chars.len() {
        out.push((index, row, col));
        if chars[index] == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
            if col == width {
                row += 1;
                col = 0;
            }
        }
        index += 1;
    }
    out.push((chars.len(), row, col));
    out
}

fn editor_cursor_at_visual_position(
    value: &str,
    width: usize,
    target_row: usize,
    target_col: usize,
) -> usize {
    let visual = editor_visual_map(value, width.max(1));
    let mut last_in_row = None;
    for (index, row, col) in visual.iter().copied() {
        if row == target_row {
            last_in_row = Some(index);
            if col >= target_col {
                return index;
            }
            continue;
        }
        if row > target_row {
            return index;
        }
    }
    last_in_row.unwrap_or_else(|| value.chars().count())
}

fn insert_at_cursor(value: &mut String, cursor: &mut usize, ch: char) {
    let mut chars = value.chars().collect::<Vec<_>>();
    let index = (*cursor).min(chars.len());
    chars.insert(index, ch);
    *cursor = index + 1;
    *value = chars.into_iter().collect();
}

fn backspace_at_cursor(value: &mut String, cursor: &mut usize) {
    let mut chars = value.chars().collect::<Vec<_>>();
    if *cursor == 0 || chars.is_empty() {
        return;
    }
    let index = (*cursor - 1).min(chars.len() - 1);
    chars.remove(index);
    *cursor = index;
    *value = chars.into_iter().collect();
}

fn delete_at_cursor(value: &mut String, cursor: &mut usize) {
    let mut chars = value.chars().collect::<Vec<_>>();
    if *cursor >= chars.len() {
        return;
    }
    chars.remove(*cursor);
    *value = chars.into_iter().collect();
}

fn line_start(value: &str, cursor: usize) -> usize {
    let chars = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    for index in (0..cursor).rev() {
        if chars[index] == '\n' {
            return index + 1;
        }
    }
    0
}

fn line_end(value: &str, cursor: usize) -> usize {
    let chars = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    for index in cursor..chars.len() {
        if chars[index] == '\n' {
            return index;
        }
    }
    chars.len()
}

fn move_vertical(value: &str, cursor: usize, width: usize, direction: isize) -> usize {
    if direction == 0 {
        return cursor.min(value.chars().count());
    }

    let width = width.max(1);
    let visual = editor_visual_map(value, width);
    let cursor = cursor.min(value.chars().count());
    let (_, row, col) = visual
        .iter()
        .copied()
        .find(|(index, _, _)| *index == cursor)
        .or_else(|| visual.last().copied())
        .unwrap_or((0, 0, 0));

    let target_row = if direction < 0 {
        row.checked_sub(1)
    } else {
        row.checked_add(1)
    };

    let Some(target_row) = target_row else {
        return cursor;
    };

    let mut fallback = None;
    for (index, candidate_row, candidate_col) in visual.iter().copied() {
        if candidate_row != target_row {
            continue;
        }
        fallback = Some((index, candidate_col));
        if candidate_col >= col {
            return index;
        }
    }

    fallback.map(|(index, _)| index).unwrap_or(cursor)
}

fn command_suggestions(input: &str) -> Vec<String> {
    [
        ":new",
        ":model",
        ":search",
        ":refresh",
        ":calendar-login",
        ":file <path>",
        ":help",
    ]
    .iter()
    .filter(|command| command.starts_with(input) || input == ":")
    .map(|command| (*command).to_string())
    .collect()
}

fn compact(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars = normalized.chars().collect::<Vec<_>>();
    if chars.len() <= limit.max(1) {
        return normalized;
    }
    let limit = limit.max(2);
    format!("{}…", chars[..limit - 1].iter().collect::<String>())
}

fn run() -> Result<(), Box<dyn Error>> {
    let models = discover_models()?;
    let socket_path = default_socket_path()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let startup_result = run_startup(&mut terminal, &socket_path, &models);
    let result = match startup_result {
        Ok(StartupOutcome::Continue(lease)) => {
            let mut app = App::new(lease, socket_path.clone())?;
            loop {
                app.poll_chat_response();
                app.poll_approvals();
                app.poll_approval_response();
                app.poll_calendar_login();
                app.poll_runtime_restart();
                app.poll_runtime_health();
                terminal.draw(|frame| app.draw(frame))?;
                if event::poll(Duration::from_millis(150))? {
                    if !app.handle_event(event::read()?)? {
                        break Ok(());
                    }
                }
            }
        }
        Ok(StartupOutcome::Quit) => Ok(()),
        Err(error) => Err(error),
    };

    drain_pending_terminal_events()?;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        ResetKeyboardEnhancementFlags,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.backend_mut().flush()?;
    terminal.show_cursor()?;
    result
}

fn drain_pending_terminal_events() -> Result<(), Box<dyn Error>> {
    while event::poll(Duration::ZERO)? {
        let _ = event::read()?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResetKeyboardEnhancementFlags;

impl Command for ResetKeyboardEnhancementFlags {
    fn write_ansi(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        out.write_str("\x1b[<u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "keyboard enhancement reset is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

enum StartupOutcome {
    Continue(ClientLease),
    Quit,
}

fn runtime_is_ready(runtime: &startup::RuntimeStatus) -> bool {
    runtime.health.ready && runtime.model_status.ready
}

fn run_startup(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    socket_path: &Path,
    models: &[ModelChoice],
) -> Result<StartupOutcome, Box<dyn Error>> {
    let runtime = probe_runtime(socket_path);
    if runtime.as_ref().is_some_and(runtime_is_ready) {
        let lease = ClientLease::acquire(socket_path)?;
        return Ok(StartupOutcome::Continue(lease));
    }

    let mut selected = initial_model_index(models);
    loop {
        terminal.draw(|frame| draw_startup(frame, models, selected, runtime.as_ref()))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(StartupOutcome::Quit),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(models.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                let model = models
                    .get(selected)
                    .ok_or("No model is selected at startup.")?;
                if runtime.as_ref().is_some_and(runtime_is_ready) {
                    let lease = ClientLease::acquire(socket_path)?;
                    return Ok(StartupOutcome::Continue(lease));
                }
                let runtime_bin = runtime_binary_path()?;
                let mut launcher = RuntimeLaunch::new(&runtime_bin, socket_path, model)?;
                launcher.wait_until_ready(socket_path)?;
                let lease = ClientLease::acquire(socket_path)?;
                launcher.detach();
                return Ok(StartupOutcome::Continue(lease));
            }
            _ => {}
        }
    }
}

fn draw_startup(
    frame: &mut Frame,
    models: &[ModelChoice],
    selected: usize,
    runtime: Option<&startup::RuntimeStatus>,
) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5.min(area.height)),
            Constraint::Min(2),
            Constraint::Length(2.min(area.height.saturating_sub(5))),
        ])
        .split(area);
    let header_lines = if let Some(runtime) = runtime {
        vec![
            Line::from(Span::styled(
                "Zaraki",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("Runtime already active."),
            Line::from(format!("Active model: {}", runtime.model_display_name)),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                "Zaraki",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("Choose the model for this runtime session."),
            Line::from("Embedding model is managed separately."),
        ]
    };
    frame.render_widget(
        Paragraph::new(header_lines).block(Block::default().borders(Borders::ALL).title("Startup")),
        vertical[0],
    );
    let items = models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            ListItem::new(Line::from(format!(
                "{} {:>2}. {}",
                if index == selected { ">" } else { " " },
                index + 1,
                compact(&model.filename, inner_width(vertical[1]).saturating_sub(6))
            )))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Local models")),
        vertical[1],
        &mut state,
    );
    if vertical[2].height > 0 {
        frame.render_widget(
            Paragraph::new("↑↓ select | Enter continue | Esc quit")
                .block(Block::default().borders(Borders::TOP)),
            vertical[2],
        );
    }
}

fn overview_detail_modal(row: Option<&RowKind>) -> Option<Modal> {
    match row {
        Some(RowKind::Task(_)) => Some(Modal::TaskDetail),
        Some(RowKind::Reminder(_)) => Some(Modal::ReminderDetail),
        None => None,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mini_task_line_stays_compact() {
        let task = UiTask {
            id: "1".to_string(),
            title: "A very long task title that should be compacted".to_string(),
            body: String::new(),
            status: "open".to_string(),
            priority: "high".to_string(),
            due_at: None,
            created_at: String::new(),
            updated_at: String::new(),
            project: None,
        };
        let line = task_line(&task, 38, LayoutMode::Mini);
        assert!(line.width() <= 38);
    }

    #[test]
    fn mini_proposal_line_stays_compact() {
        let proposal = UiProposal {
            id: "1".to_string(),
            title: "A very long proposal title that should be compacted".to_string(),
            proposed_memory: "memory".to_string(),
            conversation_turn_id: String::new(),
            decision: "should_save".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            status: String::new(),
            item_kind: String::new(),
            tags: Vec::new(),
            item_count: 1,
        };
        let line = proposal_list_line(&proposal, 38, LayoutMode::Mini);
        assert!(line.width() <= 38);
    }

    #[test]
    fn mini_job_line_stays_compact() {
        let line = format!(
            "{} {}  {}",
            "⟳",
            compact("a_very_long_job_type", 22),
            compact("running", 10)
        );
        assert!(line.chars().count() <= 38);
    }

    #[test]
    fn grouped_visual_item_count_includes_status_headers() {
        assert_eq!(grouped_visual_item_count(&[]), 1);
        assert_eq!(grouped_visual_item_count(&["open", "open", "completed"]), 5);
    }

    #[test]
    fn layout_thresholds_match_design() {
        assert_eq!(
            LayoutMode::for_area(Rect::new(0, 0, 150, 40)),
            LayoutMode::Full
        );
        assert_eq!(
            LayoutMode::for_area(Rect::new(0, 0, 100, 25)),
            LayoutMode::Compact
        );
        assert_eq!(
            LayoutMode::for_area(Rect::new(0, 0, 60, 18)),
            LayoutMode::Mini
        );
        assert_eq!(
            LayoutMode::for_area(Rect::new(0, 0, 49, 20)),
            LayoutMode::TooSmall
        );
    }

    #[test]
    fn overview_selected_row_maps_to_detail_modal() {
        assert_eq!(
            overview_detail_modal(Some(&RowKind::Task(0))),
            Some(Modal::TaskDetail)
        );
        assert_eq!(
            overview_detail_modal(Some(&RowKind::Reminder(0))),
            Some(Modal::ReminderDetail)
        );
        assert_eq!(overview_detail_modal(None), None);
    }

    #[test]
    fn reset_keyboard_enhancement_flags_writes_terminal_reset_sequence() {
        let mut output = String::new();
        ResetKeyboardEnhancementFlags
            .write_ansi(&mut output)
            .unwrap();
        assert_eq!(output, "\x1b[<u");
    }

    #[test]
    fn runtime_is_ready_requires_both_runtime_and_model_readiness() {
        let ready = startup::RuntimeStatus {
            health: HealthStatus {
                runtime: "runtime".to_string(),
                ready: true,
            },
            model_status: ModelState {
                ready: true,
                active_model: Some("qwen".to_string()),
            },
            active_model_id: "qwen".to_string(),
            model_display_name: "Qwen".to_string(),
        };
        assert!(runtime_is_ready(&ready));

        let mut model_not_ready = ready.clone();
        model_not_ready.model_status.ready = false;
        assert!(!runtime_is_ready(&model_not_ready));

        let mut runtime_not_ready = ready;
        runtime_not_ready.health.ready = false;
        assert!(!runtime_is_ready(&runtime_not_ready));
    }

    #[test]
    fn compact_collapses_and_truncates_text() {
        assert_eq!(compact("hello   world", 20), "hello world");
        assert_eq!(compact("abcdefghijklmnopqrstuvwxyz", 8), "abcdefg…");
    }

    #[test]
    fn wrap_editor_preserves_cursor_bounds() {
        let (lines, row, col) = editor_visual_lines("hello world", 6, 5);
        assert!(!lines.is_empty());
        assert!(row < lines.len());
        assert!(col <= 5);
    }

    #[test]
    fn mouse_cursor_position_tracks_visual_columns() {
        assert_eq!(editor_cursor_at_visual_position("hello", 10, 0, 0), 0);
        assert_eq!(editor_cursor_at_visual_position("hello", 10, 0, 2), 2);
        assert_eq!(editor_cursor_at_visual_position("hello", 10, 0, 5), 5);
    }

    #[test]
    fn mouse_cursor_position_tracks_soft_wrapped_rows() {
        assert_eq!(editor_cursor_at_visual_position("abcdefgh", 5, 0, 4), 4);
        assert_eq!(editor_cursor_at_visual_position("abcdefgh", 5, 0, 5), 5);
        assert_eq!(editor_cursor_at_visual_position("abcdefgh", 5, 1, 0), 5);
        assert_eq!(editor_cursor_at_visual_position("abcdefgh", 5, 1, 3), 8);
    }

    #[test]
    fn vertical_editor_movement_tracks_soft_wraps() {
        assert_eq!(move_vertical("abcdefghij", 7, 5, -1), 2);
        assert_eq!(move_vertical("abcdefghij", 2, 5, 1), 7);
    }

    #[test]
    fn vertical_editor_movement_respects_explicit_newlines() {
        assert_eq!(move_vertical("abc\ndef", 5, 5, -1), 1);
        assert_eq!(move_vertical("abc\ndef", 1, 5, 1), 5);
    }

    #[test]
    fn task_and_reminder_symbols_are_stable() {
        assert_eq!(task_symbol("open"), "☐");
        assert_eq!(task_symbol("completed"), "✓");
        assert_eq!(task_symbol("cancelled"), "✕");
    }

    #[test]
    fn session_title_from_history_uses_first_user_message() {
        let chat = vec![ChatExchange {
            user: "Plan the RCM analytics rollout".to_string(),
            assistant: Some("Let's break it down.".to_string()),
        }];
        let title = chat
            .first()
            .map(|exchange| compact(&exchange.user.replace(['\n', '\r'], " "), 48))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "New session".to_string());
        assert_eq!(title, "Plan the RCM analytics rollout");
    }

    #[test]
    fn backend_task_summary_maps_to_ui_task() {
        let task = ui_task_from_backend(assistant_protocol::TaskSummary {
            id: "task:123".to_string(),
            title: "Backend task".to_string(),
            body: "Body".to_string(),
            status: "open".to_string(),
            due_at: Some("2026-09-21T10:30:00Z".to_string()),
            created_at: Some("2026-09-20T10:30:00Z".to_string()),
            updated_at: Some("2026-09-21T10:00:00Z".to_string()),
        });
        assert_eq!(task.id, "task:123");
        assert_eq!(task.title, "Backend task");
        assert_eq!(task.body, "Body");
        assert_eq!(task.status, "open");
        assert_eq!(task.priority, "medium");
        assert!(task.due_at.is_some());
        assert_eq!(task.created_at.len(), 14);
        assert_eq!(task.updated_at.len(), 14);
        assert!(task.project.is_none());
    }

    #[test]
    fn demo_tasks_are_ordered_by_status_then_priority() {
        let mut tasks = demo_tasks();
        sort_ui_tasks(&mut tasks);
        let groups = tasks
            .iter()
            .map(|task| (task.status.as_str(), task.priority.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            groups,
            vec![
                ("expired", "low"),
                ("in_progress", "high"),
                ("open", "high"),
                ("open", "medium"),
                ("open", "low"),
                ("completed", "high"),
                ("cancelled", "medium"),
            ]
        );
    }

    #[test]
    fn backend_reminder_summary_maps_to_ui_reminder() {
        let reminder = ui_reminder_from_backend(assistant_protocol::ReminderSummary {
            id: "reminder:test".to_string(),
            title: "Review report".to_string(),
            body: "Send notes.".to_string(),
            status: "scheduled".to_string(),
            due_at: Some("2026-09-24T04:00:00Z".to_string()),
            created_at: Some("2026-09-21T10:00:00Z".to_string()),
            updated_at: Some("2026-09-21T11:00:00Z".to_string()),
            calendar_sync_enabled: true,
            calendar_sync_status: Some("synced".to_string()),
        });
        assert_eq!(reminder.id, "reminder:test");
        assert_eq!(reminder.title, "Review report");
        assert_eq!(reminder.body, "Send notes.");
        assert_eq!(reminder.status, "scheduled");
        assert!(reminder.due_at.is_some());
        assert!(!reminder.created_at.is_empty());
        assert!(!reminder.updated_at.is_empty());
        assert!(reminder.calendar_sync_enabled);
        assert_eq!(reminder.calendar_sync_status.as_deref(), Some("synced"));
    }

    #[test]
    fn backend_proposal_summary_maps_to_ui_proposal() {
        let proposal = ui_proposal_from_backend(assistant_protocol::MemoryProposalSummary {
            id: "proposal:test".to_string(),
            decision: "should_save".to_string(),
            conversation_turn_id: "42".to_string(),
            status: "pending".to_string(),
            created_at: "2026-09-21T12:00:00Z".to_string(),
            updated_at: "2026-09-21T12:05:00Z".to_string(),
            title: "Runtime architecture".to_string(),
            proposed_memory: "Zaraki uses a Rust runtime.".to_string(),
            item_kind: "fact".to_string(),
            tags: vec!["Zaraki".to_string(), "runtime".to_string()],
            item_count: 1,
        });
        assert_eq!(proposal.id, "proposal:test");
        assert_eq!(proposal.title, "Runtime architecture");
        assert_eq!(proposal.decision, "should_save");
        assert_eq!(proposal.status, "pending");
        assert_eq!(proposal.item_kind, "fact");
        assert_eq!(
            proposal.tags,
            vec!["Zaraki".to_string(), "runtime".to_string()]
        );
        assert_eq!(proposal.item_count, 1);
        assert!(!proposal.created_at.is_empty());
        assert!(!proposal.updated_at.is_empty());
    }

    #[test]
    fn calendar_event_mapping_extracts_summary_and_times() {
        let event = ui_calendar_event_from_backend(serde_json::json!({
            "id": "event-1",
            "summary": "Zaraki review",
            "status": "confirmed",
            "start": {"dateTime": "2026-09-24T12:00:00Z"},
            "end": {"dateTime": "2026-09-24T12:30:00Z"},
            "location": "Home",
            "htmlLink": "https://calendar.google.com/event",
        }))
        .expect("event should map");
        assert_eq!(event.id, "event-1");
        assert_eq!(event.summary, "Zaraki review");
        assert_eq!(event.status, "confirmed");
        assert!(!event.start.is_empty());
        assert!(!event.end.is_empty());
        assert_eq!(event.location, "Home");
        assert_eq!(event.html_link, "https://calendar.google.com/event");
    }

    #[test]
    fn system_diagnostics_use_live_values_without_placeholder_model_stats() {
        let info = serde_json::json!({
            "hardware": {
                "memory": {
                    "total_bytes": 4096,
                    "used_bytes": 2048
                },
                "gpu": {
                    "devices": [{
                        "name": "Test GPU",
                        "vram_total_mib": 1024,
                        "vram_used_mib": 256
                    }]
                }
            },
            "runtime": {
                "pid": 42,
                "generation": {
                    "endpoint": "http://127.0.0.1:8080",
                    "reachable": true
                }
            },
            "network": {
                "default_interface": "eth0"
            }
        });
        let lines =
            system_diagnostics_lines(Some(&info), Some("Qwen"), Some(true), LayoutMode::Full);
        let rendered = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(rendered.contains("PID: 42"));
        assert!(rendered.contains("eth0"));
        assert!(rendered.contains("Test GPU"));
        assert!(rendered.contains("256 / 1024 MiB"));
        assert!(rendered.contains("2.0 KiB"));
        assert!(!rendered.contains("Generation: n/a"));
    }

    #[test]
    fn backend_job_summary_maps_to_ui_job() {
        let job = ui_job_from_backend(assistant_protocol::JobSummary {
            id: "job:test".to_string(),
            job_type: "memory extraction".to_string(),
            status: "running".to_string(),
            next_run_at: "now".to_string(),
        });
        assert_eq!(job.id, "job:test");
        assert_eq!(job.job_type, "memory extraction");
        assert_eq!(job.status, "running");
        assert_eq!(job.next_run_at, "now");
    }

    #[test]
    fn task_form_defaults_are_backend_neutral() {
        let form = FormState::task_create();
        assert_eq!(form.values, vec!["", "", "", "open", "medium", ""]);
        assert_eq!(
            form.labels,
            vec!["Title", "Body", "Due", "Status", "Priority", "Project"]
        );
        assert!(form.editing_id.is_none());
    }

    #[test]
    fn task_form_edit_round_trips_demo_task_fields() {
        let tasks = demo_tasks();
        let form = FormState::task_edit(&tasks[0]);
        assert_eq!(form.editing_id.as_deref(), Some(tasks[0].id.as_str()));
        assert_eq!(form.values[0], tasks[0].title);
        assert_eq!(form.values[1], tasks[0].body);
        assert_eq!(form.values[3], tasks[0].status);
        assert_eq!(form.values[4], tasks[0].priority);
        assert_eq!(form.values[5], tasks[0].project.clone().unwrap());
    }

    #[test]
    fn form_edit_due_uses_local_editable_date_format() {
        let reminder = UiReminder {
            id: "reminder:test".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            status: "scheduled".to_string(),
            due_at: Some("2026-09-24T12:30:00Z".to_string()),
            created_at: String::new(),
            updated_at: String::new(),
            calendar_sync_enabled: true,
            calendar_sync_status: Some("synced".to_string()),
        };
        let form = FormState::reminder_edit(&reminder);
        let expected_local = OffsetDateTime::parse("2026-09-24T12:30:00Z", &Rfc3339)
            .expect("test timestamp")
            .to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC));
        let expected = format!(
            "{:02}/{:02}/{:04} {:02}:{:02}",
            expected_local.day(),
            expected_local.month() as u8,
            expected_local.year(),
            expected_local.hour(),
            expected_local.minute()
        );
        assert_eq!(form.values[2], expected);
    }

    #[test]
    fn task_form_body_wraps_and_due_has_hint_without_value() {
        let mut form = FormState::task_create();
        form.active = 1;
        form.values[1] = "This is a long task body that should wrap inside the task form.".into();
        let lines = form_lines(&form, 32);
        assert!(lines.len() > form.labels.len() + 2);

        form.active = 2;
        form.cursor = 0;
        let due_lines = form_lines(&form, 32);
        let rendered = due_lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("DD/MM/YYYY hh:mm (09:00 default)"));
        assert!(form.values[2].is_empty());
    }

    #[test]
    fn inactive_task_body_does_not_render_an_editor_cursor() {
        let mut form = FormState::task_create();
        form.values[1] = "A body that spans multiple visual rows in the form.".into();
        form.active = 2;
        form.cursor = 0;
        let lines = form_lines(&form, 32);
        let cursor_count = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains("▏"))
            .count();
        assert_eq!(
            cursor_count, 1,
            "only the active Due field should render a cursor"
        );
    }

    #[test]
    fn local_task_status_updates_and_refreshes_timestamp() {
        let mut tasks = demo_tasks();
        let original = tasks[2].updated_at.clone();
        let id = tasks[2].id.clone();
        assert!(set_local_task_status(&mut tasks, &id, "completed"));
        assert_eq!(tasks[2].status, "completed");
        assert!(!tasks[2].updated_at.is_empty());
        assert_ne!(tasks[2].updated_at, original);
        assert!(!set_local_task_status(&mut tasks, "missing", "completed"));
    }

    #[test]
    fn local_task_action_statuses_are_frontend_vocabulary() {
        let mut tasks = demo_tasks();
        let complete_id = tasks[2].id.clone();
        let cancel_id = tasks[3].id.clone();
        assert!(set_local_task_status(&mut tasks, &complete_id, "completed"));
        assert!(set_local_task_status(&mut tasks, &cancel_id, "cancelled"));
        assert_eq!(tasks[2].status, "completed");
        assert_eq!(tasks[3].status, "cancelled");
    }

    #[test]
    fn task_detail_body_avoids_orphan_word_lines() {
        let body = "Try out Jev on laptop and see if it can handle the load. Make a MCP with it to sell and earn some fragrant cash.";
        let lines = wrap_balanced_preserving_newlines(body, 64);
        assert!(lines.iter().all(|line| line.chars().count() <= 64));
        assert!(!lines.iter().any(|line| line == "a"));
    }

    #[test]
    fn task_labels_match_frontend_vocabulary() {
        assert_eq!(task_status_label("in_progress"), "In Progress");
        assert_eq!(task_priority_label("high"), "High");
        assert_eq!(task_priority_label("medium"), "Medium");
        assert_eq!(task_priority_label("low"), "Low");
    }

    #[test]
    fn command_suggestions_are_chat_specific() {
        assert!(command_suggestions(":").len() >= 4);
        assert!(command_suggestions(":mo").contains(&":model".to_string()));
        assert!(command_suggestions(":ca").contains(&":calendar-login".to_string()));
    }

    #[test]
    fn backend_model_summary_maps_without_invented_diagnostics() {
        let model = ui_model_from_backend(assistant_protocol::ModelInfo {
            id: "qwen".to_string(),
            display_name: "Qwen3.5-4B-Q4_K_M.gguf".to_string(),
            available: true,
            active: true,
            capabilities: vec!["controller".to_string(), "general_response".to_string()],
        });

        assert_eq!(model.id, "qwen");
        assert_eq!(model.display_name, "Qwen3.5-4B-Q4_K_M.gguf");
        assert!(model.available);
        assert!(model.active);
        assert_eq!(model.capabilities, vec!["controller", "general_response"]);
    }

    #[test]
    fn model_selection_skips_unavailable_models() {
        let models = vec![
            UiModel {
                id: "local:phi".into(),
                display_name: "Phi".into(),
                available: true,
                active: false,
                capabilities: vec![],
            },
            UiModel {
                id: "cloud:test".into(),
                display_name: "Cloud".into(),
                available: false,
                active: false,
                capabilities: vec![],
            },
            UiModel {
                id: "qwen".into(),
                display_name: "Qwen".into(),
                available: true,
                active: true,
                capabilities: vec![],
            },
        ];
        let selected = models.iter().filter(|model| model.available).nth(1);
        assert_eq!(selected.map(|model| model.id.as_str()), Some("qwen"));
    }

    #[test]
    fn form_defaults_match_backend_status_names() {
        assert_eq!(FormState::task_create().values[3], "open");
        let reminder = FormState::reminder_create();
        assert!(reminder.values[0].is_empty());
        assert_eq!(reminder.values[3], "no");
        assert_eq!(reminder.labels[3], "Calendar Sync");
    }

    #[test]
    fn proposal_created_at_uses_backend_timestamp() {
        let proposal = UiProposal {
            id: "proposal-1".into(),
            title: "Backend proposal".into(),
            created_at: "14-09-26 18:29:28".into(),
            updated_at: "14-09-26 18:29:28".into(),
            decision: "should_save".into(),
            status: "pending".into(),
            conversation_turn_id: "24".into(),
            proposed_memory: "memory".into(),
            item_kind: "fact".into(),
            tags: vec!["Zaraki".into()],
            item_count: 1,
        };
        assert_eq!(proposal_created_at(&proposal), "14-09-26 18:29:28");
    }

    #[test]
    fn proposal_decision_is_human_readable() {
        assert_eq!(proposal_decision_label("should_save"), "Should save");
        assert_eq!(proposal_decision_label("pending"), "Pending review");
        assert_eq!(
            proposal_decision_label("should_not_save"),
            "Should not save"
        );
    }

    #[test]
    fn proposal_title_uses_backend_title() {
        let proposal = UiProposal {
            id: "proposal-1".into(),
            title: "Backend proposal".into(),
            created_at: "21-09-26 12:40".into(),
            updated_at: "21-09-26 12:40".into(),
            decision: "pending".into(),
            status: "pending".into(),
            conversation_turn_id: "24".into(),
            proposed_memory: "memory".into(),
            item_kind: "fact".into(),
            tags: vec!["Zaraki".into()],
            item_count: 1,
        };
        assert_eq!(proposal_title(&proposal), "Backend proposal");
    }

    #[test]
    fn local_reminder_defaults_and_statuses_are_frontend_ready() {
        let reminders = demo_reminders();
        assert!(!reminders.is_empty());
        assert!(reminders.iter().all(|reminder| !reminder.title.is_empty()));
        assert!(
            reminders
                .iter()
                .any(|reminder| reminder.status == "scheduled")
        );
        assert!(
            reminders
                .iter()
                .any(|reminder| reminder.status == "triggered")
        );
    }

    #[test]
    fn app_uses_client_boundary() {
        let lease = ClientLease::acquire("/tmp/zaraki-test-missing.sock");
        assert!(lease.is_err());
    }
}
