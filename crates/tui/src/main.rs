mod startup;

use std::{error::Error, io, path::PathBuf, time::Duration};

use startup::{
    ModelChoice, RuntimeLaunch, discover_models, initial_model_index, probe_runtime,
    runtime_binary_path,
};

use assistant_client::{ClientLease, IpcClient, IpcClientError, default_socket_path};
use assistant_protocol::{
    ChatResponse, HealthStatus, JobSummary, MemoryProposalSummary, ModelInfo, ModelState,
    ReminderSummary, RequestMethod, ResponsePayload, TaskSummary,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

const TABS: [&str; 8] = [
    "Overview",
    "Tasks",
    "Reminders",
    "Proposals",
    "Chat",
    "Models",
    "Jobs",
    "Search",
];

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
            _ => Self::Search,
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
    assistant: String,
}

struct App {
    client: IpcClient,
    socket_path: PathBuf,
    health: Option<HealthStatus>,
    model_status: Option<ModelState>,
    models: Vec<ModelInfo>,
    tasks: Vec<TaskSummary>,
    reminders: Vec<ReminderSummary>,
    proposals: Vec<MemoryProposalSummary>,
    jobs: Vec<JobSummary>,
    chat: Vec<ChatExchange>,
    tab: Tab,
    selected: usize,
    input_mode: InputMode,
    input: String,
    search_results: Vec<(String, String, f64)>,
    message: String,
}

impl App {
    fn new() -> Result<Self, Box<dyn Error>> {
        let socket_path = default_socket_path()?;
        let client = IpcClient::new(&socket_path);
        let mut app = Self::from_client(client, socket_path);
        if let Err(error) = app.refresh() {
            app.message = format!("Assistant runtime unavailable: {error}");
        }
        Ok(app)
    }

    fn from_client(client: IpcClient, socket_path: PathBuf) -> Self {
        Self {
            client,
            socket_path,
            health: None,
            model_status: None,
            models: Vec::new(),
            tasks: Vec::new(),
            reminders: Vec::new(),
            proposals: Vec::new(),
            jobs: Vec::new(),
            chat: Vec::new(),
            tab: Tab::Overview,
            selected: 0,
            input_mode: InputMode::None,
            input: String::new(),
            search_results: Vec::new(),
            message: String::new(),
        }
    }

    fn refresh(&mut self) -> Result<(), IpcClientError> {
        self.health = Some(self.request_health()?);
        self.model_status = Some(self.request_model_status()?);
        self.models = self.request_models()?;
        self.tasks = self.request_tasks()?;
        self.reminders = self.request_reminders()?;
        self.proposals = self.request_proposals()?;
        self.jobs = self.request_jobs()?;
        if self.tab == Tab::Search && !self.input.trim().is_empty() {
            self.search_results = self.request_search(&self.input)?;
        } else if self.tab != Tab::Search {
            self.search_results.clear();
        }
        self.selected = self
            .current_len()
            .checked_sub(1)
            .map_or(0, |last| self.selected.min(last));
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

    fn request_models(&self) -> Result<Vec<ModelInfo>, IpcClientError> {
        match self.client.request(RequestMethod::ModelList)? {
            ResponsePayload::Models(value) => Ok(value.models),
            other => Err(unexpected_response("model list", other)),
        }
    }

    fn request_tasks(&self) -> Result<Vec<TaskSummary>, IpcClientError> {
        match self.client.request(RequestMethod::TasksList {
            status: None,
            limit: Some(200),
        })? {
            ResponsePayload::Tasks(value) => Ok(value.tasks),
            other => Err(unexpected_response("tasks", other)),
        }
    }

    fn request_reminders(&self) -> Result<Vec<ReminderSummary>, IpcClientError> {
        match self.client.request(RequestMethod::RemindersList {
            status: None,
            limit: Some(200),
        })? {
            ResponsePayload::Reminders(value) => Ok(value.reminders),
            other => Err(unexpected_response("reminders", other)),
        }
    }

    fn request_proposals(&self) -> Result<Vec<MemoryProposalSummary>, IpcClientError> {
        match self
            .client
            .request(RequestMethod::MemoryProposals { limit: Some(50) })?
        {
            ResponsePayload::Proposals(value) => Ok(value.proposals),
            other => Err(unexpected_response("memory proposals", other)),
        }
    }

    fn request_jobs(&self) -> Result<Vec<JobSummary>, IpcClientError> {
        match self.client.request(RequestMethod::JobsList {
            status: None,
            limit: Some(100),
        })? {
            ResponsePayload::Jobs(value) => Ok(value.jobs),
            other => Err(unexpected_response("jobs", other)),
        }
    }

    fn request_search(&self, query: &str) -> Result<Vec<(String, String, f64)>, IpcClientError> {
        match self.client.request(RequestMethod::MemorySearch {
            query: query.to_string(),
            limit: Some(50),
        })? {
            ResponsePayload::Memory(value) => Ok(value
                .results
                .into_iter()
                .map(|item| (item.id, item.text, item.score))
                .collect()),
            other => Err(unexpected_response("memory search", other)),
        }
    }

    fn current_len(&self) -> usize {
        match self.tab {
            Tab::Overview => 0,
            Tab::Tasks => self.tasks.len(),
            Tab::Reminders => self.reminders.len(),
            Tab::Proposals => self.proposals.len(),
            Tab::Chat => self.chat.len(),
            Tab::Models => self.models.len(),
            Tab::Jobs => self.jobs.len(),
            Tab::Search => self.search_results.len(),
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = 0;
        self.input_mode = InputMode::None;
        self.input.clear();
        self.message.clear();
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
            self.selected.saturating_add(delta as usize)
        };
        self.selected = next.min(len - 1);
    }

    fn accept_selected_proposal(&mut self) {
        if self.tab != Tab::Proposals || self.proposals.is_empty() {
            return;
        }
        let id = self.proposals[self.selected].id.clone();
        match self
            .client
            .request(RequestMethod::MemoryProposalAccept { id: id.clone() })
        {
            Ok(ResponsePayload::Mutation(result)) => {
                self.message = format!("Accepted {id}: {}", compact_json(&result.output));
                if let Err(error) = self.refresh() {
                    self.message = format!("Accepted {id}, refresh failed: {error}");
                }
            }
            Ok(other) => self.message = format!("Unexpected accept response: {other:?}"),
            Err(error) => self.message = format!("Accept failed: {error}"),
        }
    }

    fn reject_selected_proposal(&mut self) {
        if self.tab != Tab::Proposals || self.proposals.is_empty() {
            return;
        }
        let id = self.proposals[self.selected].id.clone();
        match self
            .client
            .request(RequestMethod::MemoryProposalReject { id: id.clone() })
        {
            Ok(ResponsePayload::Mutation(result)) => {
                self.message = format!("Rejected {id}: {}", compact_json(&result.output));
                if let Err(error) = self.refresh() {
                    self.message = format!("Rejected {id}, refresh failed: {error}");
                }
            }
            Ok(other) => self.message = format!("Unexpected reject response: {other:?}"),
            Err(error) => self.message = format!("Reject failed: {error}"),
        }
    }

    fn switch_selected_model(&mut self) {
        if self.tab != Tab::Models || self.models.is_empty() {
            return;
        }

        let model = self.models[self.selected].id.clone();
        match self.client.request(RequestMethod::ModelSwitch {
            model: model.clone(),
        }) {
            Ok(ResponsePayload::ModelStatus(status)) => {
                self.model_status = Some(status);
                match self.refresh() {
                    Ok(()) => self.message = format!("Switched to {model}."),
                    Err(error) => self.message = format!("Model switched, refresh failed: {error}"),
                }
            }
            Ok(other) => self.message = format!("Unexpected model switch response: {other:?}"),
            Err(error) => self.message = format!("Model switch failed: {error}"),
        }
    }

    fn run_search(&mut self) {
        self.input_mode = InputMode::None;
        let query = self.input.trim().to_string();
        if query.is_empty() {
            self.search_results.clear();
            self.message = "Search query is empty.".to_string();
            return;
        }
        match self.request_search(&query) {
            Ok(results) => {
                self.search_results = results;
                self.selected = 0;
                self.message = format!("{} search result(s).", self.search_results.len());
            }
            Err(error) => self.message = format!("Search failed: {error}"),
        }
    }

    fn send_chat(&mut self) {
        self.input_mode = InputMode::None;
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return;
        }

        match self
            .client
            .request(RequestMethod::Chat { text: text.clone() })
        {
            Ok(ResponsePayload::Chat(ChatResponse { text: response })) => {
                self.chat.push(ChatExchange {
                    user: text,
                    assistant: response,
                });
                self.selected = self.chat.len().saturating_sub(1);
                self.message.clear();
            }
            Ok(other) => self.message = format!("Unexpected chat response: {other:?}"),
            Err(error) => self.message = format!("Chat failed: {error}"),
        }
    }

    fn handle_event(&mut self, event: Event) -> Result<bool, Box<dyn Error>> {
        let Event::Key(key) = event else {
            return Ok(true);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(true);
        }

        match self.input_mode {
            InputMode::Search | InputMode::Chat => {
                match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::None;
                        self.input.clear();
                    }
                    KeyCode::Enter => match self.input_mode {
                        InputMode::Search => self.run_search(),
                        InputMode::Chat => self.send_chat(),
                        InputMode::None => unreachable!(),
                    },
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.input.push(c)
                    }
                    _ => {}
                }
                return Ok(true);
            }
            InputMode::None => {}
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Tab => self.next_tab(),
            KeyCode::BackTab => self.previous_tab(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('r') => match self.refresh() {
                Ok(()) => self.message = "Refreshed from assistant runtime.".to_string(),
                Err(error) => self.message = format!("Refresh failed: {error}"),
            },
            KeyCode::Char('a') => self.accept_selected_proposal(),
            KeyCode::Char('x') => self.reject_selected_proposal(),
            KeyCode::Enter if self.tab == Tab::Models => self.switch_selected_model(),
            KeyCode::Char('/') if self.tab == Tab::Search => {
                self.input_mode = InputMode::Search;
                self.input.clear();
            }
            KeyCode::Char('c') if self.tab == Tab::Chat => {
                self.input_mode = InputMode::Chat;
                self.input.clear();
            }
            KeyCode::Char(c @ '1'..='8') => {
                self.set_tab(Tab::from_index(c as usize - '1' as usize))
            }
            _ => {}
        }
        Ok(true)
    }

    fn title(&self) -> String {
        let title = TABS[self.tab.index()];
        match self.input_mode {
            InputMode::Search => format!("Search: {}_", self.input),
            InputMode::Chat => format!("Chat: {}_", self.input),
            InputMode::None if self.tab == Tab::Proposals => {
                format!("{title}  [a accept | x reject]")
            }
            InputMode::None => title.to_string(),
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(area);

        let tabs = TABS
            .iter()
            .enumerate()
            .map(|(index, title)| {
                let style = if index == self.tab.index() {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Span::styled(format!(" {}:{} ", index + 1, title), style)
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(Line::from(tabs)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Assistant / Crona  |  {}", self.connection_title())),
            ),
            vertical[0],
        );

        match self.tab {
            Tab::Overview => self.draw_overview(frame, vertical[1]),
            Tab::Tasks => self.draw_tasks(frame, vertical[1]),
            Tab::Reminders => self.draw_reminders(frame, vertical[1]),
            Tab::Proposals => self.draw_proposals(frame, vertical[1]),
            Tab::Chat => self.draw_chat(frame, vertical[1]),
            Tab::Models => self.draw_models(frame, vertical[1]),
            Tab::Jobs => self.draw_jobs(frame, vertical[1]),
            Tab::Search => self.draw_search(frame, vertical[1]),
        }

        let footer = if self.message.is_empty() {
            match self.tab {
                Tab::Chat => "q quit | tab switch | ↑↓ select | c chat | r refresh",
                Tab::Search => "q quit | tab switch | ↑↓ select | / search | r refresh",
                Tab::Proposals => {
                    "q quit | tab switch | ↑↓ select | a accept | x reject | r refresh"
                }
                Tab::Models => "q quit | tab switch | ↑↓ select | Enter switch | r refresh",
                _ => "q quit | tab switch | ↑↓ select | r refresh",
            }
        } else {
            &self.message
        };
        frame.render_widget(
            Paragraph::new(footer).block(Block::default().borders(Borders::ALL)),
            vertical[2],
        );
    }

    fn connection_title(&self) -> String {
        match &self.health {
            Some(health) if health.ready => "● runtime ready".to_string(),
            Some(_) => "○ runtime not ready".to_string(),
            None => format!("○ {}", self.socket_path.display()),
        }
    }

    fn draw_overview(&self, frame: &mut Frame, area: Rect) {
        let health = self.health.as_ref();
        let active_model = self
            .model_status
            .as_ref()
            .and_then(|status| status.active_model.as_deref())
            .unwrap_or("unknown");
        let ready = self
            .model_status
            .as_ref()
            .map(|status| status.ready)
            .unwrap_or(false);
        let text = vec![
            Line::from(format!("Assistant socket: {}", self.socket_path.display())),
            Line::from(format!(
                "Runtime:          {}",
                health
                    .map(|value| value.runtime.as_str())
                    .unwrap_or("unavailable")
            )),
            Line::from(format!(
                "Runtime ready:    {}",
                health.map(|value| value.ready).unwrap_or(false)
            )),
            Line::from(format!("Active model:     {active_model}")),
            Line::from(format!("Model ready:      {ready}")),
            Line::from(format!("Tasks:            {}", self.tasks.len())),
            Line::from(format!("Reminders:        {}", self.reminders.len())),
            Line::from(format!("Pending proposals: {}", self.proposals.len())),
            Line::from(format!("Jobs:             {}", self.jobs.len())),
            Line::from(format!("Local chat turns: {}", self.chat.len())),
        ];
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(self.title()))
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn draw_tasks(&self, frame: &mut Frame, area: Rect) {
        let items = self.tasks.iter().map(|task| {
            ListItem::new(format!(
                "{}  [{}]  {}",
                task.due_at.as_deref().unwrap_or("-"),
                task.status,
                task.title
            ))
        });
        self.draw_list(frame, area, items.collect(), "Tasks");
    }

    fn draw_reminders(&self, frame: &mut Frame, area: Rect) {
        let items = self.reminders.iter().map(|reminder| {
            ListItem::new(format!(
                "{}  [{}]  {}",
                reminder.due_at.as_deref().unwrap_or("-"),
                reminder.status,
                reminder.title
            ))
        });
        self.draw_list(frame, area, items.collect(), "Reminders");
    }

    fn draw_proposals(&self, frame: &mut Frame, area: Rect) {
        let items = self.proposals.iter().map(|proposal| {
            ListItem::new(format!(
                "{}  [{}]  turn={}",
                proposal.id, proposal.decision, proposal.conversation_turn_id
            ))
        });
        self.draw_list(frame, area, items.collect(), &self.title());
    }

    fn draw_chat(&self, frame: &mut Frame, area: Rect) {
        if self.chat.is_empty() {
            self.draw_list(
                frame,
                area,
                vec![ListItem::new("Press c to chat with the assistant runtime.")],
                &self.title(),
            );
            return;
        }

        let items = self.chat.iter().map(|exchange| {
            ListItem::new(vec![
                Line::from(format!("You: {}", compact(&exchange.user, 160))),
                Line::from(format!("Assistant: {}", compact(&exchange.assistant, 220))),
            ])
        });
        self.draw_list(frame, area, items.collect(), &self.title());
    }

    fn draw_models(&self, frame: &mut Frame, area: Rect) {
        let active = self
            .model_status
            .as_ref()
            .and_then(|status| status.active_model.as_deref());
        let items = self.models.iter().map(|model| {
            let marker = if active == Some(model.id.as_str()) {
                '*'
            } else {
                ' '
            };
            let availability = if model.available { "ready" } else { "offline" };
            ListItem::new(format!(
                "{marker} [{}] {}  {}",
                availability, model.id, model.display_name
            ))
        });
        self.draw_list(frame, area, items.collect(), &self.title());
    }

    fn draw_jobs(&self, frame: &mut Frame, area: Rect) {
        let items = self.jobs.iter().map(|job| {
            ListItem::new(format!(
                "{}  [{}] {}  next={}",
                job.id, job.status, job.job_type, job.next_run_at
            ))
        });
        self.draw_list(frame, area, items.collect(), "Jobs");
    }

    fn draw_search(&self, frame: &mut Frame, area: Rect) {
        let mut items = Vec::new();
        if self.input.is_empty() {
            items.push(ListItem::new(
                "Press / to search assistant memory, then Enter.",
            ));
        } else if self.search_results.is_empty() {
            items.push(ListItem::new("No results."));
        } else {
            items.extend(self.search_results.iter().map(|(id, text, score)| {
                ListItem::new(vec![
                    Line::from(format!("{}  score={:.2}", id, score)),
                    Line::from(format!("  {}", compact(text, 180))),
                ])
            }));
        }
        self.draw_list(frame, area, items, &self.title());
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect, items: Vec<ListItem<'_>>, title: &str) {
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::BOLD));
        if self.current_len() == 0 {
            frame.render_widget(list, area);
            return;
        }
        let mut state = ratatui::widgets::ListState::default();
        state.select(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }
}

fn unexpected_response(method: &str, response: ResponsePayload) -> IpcClientError {
    IpcClientError::Server {
        code: "unexpected_response".to_string(),
        message: format!("Unexpected response for {method}: {response:?}"),
    }
}

fn compact(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars = normalized.chars().collect::<Vec<_>>();
    if chars.len() <= limit {
        normalized
    } else {
        format!(
            "{}…",
            chars[..limit.saturating_sub(1)].iter().collect::<String>()
        )
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_string())
}

fn run() -> Result<(), Box<dyn Error>> {
    let models = discover_models()?;
    let socket_path = default_socket_path()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let startup_result = run_startup(&mut terminal, &socket_path, &models);
    let result = match startup_result {
        Ok(StartupOutcome::Continue(_lease)) => {
            let mut app = App::new()?;
            loop {
                terminal.draw(|frame| app.draw(frame))?;
                if event::poll(Duration::from_millis(250))? && !app.handle_event(event::read()?)? {
                    break Ok(());
                }
            }
        }
        Ok(StartupOutcome::Quit) => Ok(()),
        Err(error) => Err(error),
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

enum StartupOutcome {
    Continue(ClientLease),
    Quit,
}

fn run_startup(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    socket_path: &std::path::Path,
    models: &[ModelChoice],
) -> Result<StartupOutcome, Box<dyn Error>> {
    let runtime = probe_runtime(socket_path);
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
            KeyCode::Char('q') | KeyCode::Esc => return Ok(StartupOutcome::Quit),
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(models.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let model = models
                    .get(selected)
                    .ok_or("No model is selected at startup.")?;

                if let Some(runtime) = runtime.as_ref() {
                    let lease = ClientLease::acquire(socket_path)?;
                    let local_id = format!("local:{}", model.filename);
                    let desired_id = if runtime.active_model_id == local_id {
                        local_id
                    } else if runtime.active_model_id == "qwen"
                        && runtime.model_display_name == model.filename
                    {
                        "qwen".to_string()
                    } else {
                        local_id
                    };

                    if desired_id != runtime.active_model_id {
                        let client = IpcClient::new(socket_path);
                        match client.request(assistant_protocol::RequestMethod::ModelSwitch {
                            model: desired_id.clone(),
                        })? {
                            ResponsePayload::ModelStatus(status) if status.ready => {}
                            ResponsePayload::ModelStatus(_) => {
                                return Err(format!(
                                    "Runtime switched to {desired_id}, but the selected model is not ready."
                                )
                                .into());
                            }
                            other => {
                                return Err(
                                    format!("Unexpected model switch response: {other:?}").into()
                                );
                            }
                        }
                    }

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
            Constraint::Length(5),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);

    let header = if let Some(runtime) = runtime {
        vec![
            Line::from("PERSONAL ASSISTANT"),
            Line::from("Runtime already active."),
            Line::from(format!(
                "Active model: {} ({})",
                runtime.model_display_name, runtime.active_model_id
            )),
        ]
    } else {
        vec![
            Line::from("PERSONAL ASSISTANT"),
            Line::from("Choose the model to load for this runtime session."),
            Line::from("The embedding model is managed separately by the runtime."),
        ]
    };

    frame.render_widget(
        Paragraph::new(header)
            .block(Block::default().borders(Borders::ALL).title("Startup"))
            .wrap(Wrap { trim: true }),
        vertical[0],
    );

    let items = models.iter().enumerate().map(|(index, model)| {
        let marker = if index == selected { ">" } else { " " };
        ListItem::new(format!("{marker} {:>2}. {}", index + 1, model.filename))
    });
    let list = List::new(items.collect::<Vec<_>>())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Available local models"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(list, vertical[1]);

    let footer = if runtime.is_some() {
        "↑↓ select | Enter continue with active runtime | q quit"
    } else {
        "↑↓ select | Enter launch runtime | q quit"
    };
    frame.render_widget(
        Paragraph::new(footer).block(Block::default().borders(Borders::ALL)),
        vertical[2],
    );
}

fn main() -> Result<(), Box<dyn Error>> {
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_cycle_in_order() {
        assert_eq!(Tab::from_index(0), Tab::Overview);
        assert_eq!(Tab::from_index(7), Tab::Search);
        assert_eq!(Tab::from_index(8), Tab::Overview);
    }

    #[test]
    fn compact_collapses_and_truncates_text() {
        assert_eq!(compact("hello   world", 20), "hello world");
        assert_eq!(compact("abcdefghijklmnopqrstuvwxyz", 8), "abcdefg…");
    }

    #[test]
    fn app_uses_the_assistant_client_boundary() {
        let app = App::from_client(
            IpcClient::new("/tmp/assistant-tui-test.sock"),
            PathBuf::from("/tmp/assistant-tui-test.sock"),
        );
        assert!(app.tasks.is_empty());
        assert!(app.proposals.is_empty());
        assert_eq!(
            app.socket_path,
            PathBuf::from("/tmp/assistant-tui-test.sock")
        );
    }
}
