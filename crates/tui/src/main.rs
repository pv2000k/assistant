use std::{env, error::Error, io, path::PathBuf, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use orchestrator::PersistentMemoryIndexer;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use sqlite_memory::{
    ConversationTurn, Job, JobStatus, MemoryExtractionProposal, ReminderRecord, SqliteMemoryDb,
    TaskRecord,
};

const TABS: [&str; 7] = [
    "Overview",
    "Tasks",
    "Reminders",
    "Proposals",
    "Conversation",
    "Jobs",
    "Search",
];

fn env_or_default(name: &str, default: String) -> String {
    env::var(name).unwrap_or(default)
}

fn memory_root() -> Result<PathBuf, Box<dyn Error>> {
    if let Ok(value) = env::var("ASSISTANT_MEMORY_ROOT") {
        return Ok(PathBuf::from(value));
    }
    let home = env::var("HOME").map(PathBuf::from)?;
    Ok(home.join("assistant-memory"))
}

fn db_path(memory_root: &PathBuf) -> PathBuf {
    PathBuf::from(env_or_default(
        "ASSISTANT_SQLITE_DB_PATH",
        memory_root
            .join("assistant.db")
            .to_string_lossy()
            .into_owned(),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Tasks,
    Reminders,
    Proposals,
    Conversation,
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
            Self::Conversation => 4,
            Self::Jobs => 5,
            Self::Search => 6,
        }
    }

    fn from_index(index: usize) -> Self {
        match index % TABS.len() {
            0 => Self::Overview,
            1 => Self::Tasks,
            2 => Self::Reminders,
            3 => Self::Proposals,
            4 => Self::Conversation,
            5 => Self::Jobs,
            _ => Self::Search,
        }
    }
}

struct App {
    db: SqliteMemoryDb,
    indexer: PersistentMemoryIndexer,
    memory_root: PathBuf,
    db_path: PathBuf,
    qwen_url: String,
    qwen_model: String,
    embedding_url: String,
    embedding_model: String,
    background_workers_enabled: bool,
    tab: Tab,
    selected: usize,
    search_mode: bool,
    search_query: String,
    search_results: Vec<(String, String, f64)>,
    tasks: Vec<TaskRecord>,
    reminders: Vec<ReminderRecord>,
    proposals: Vec<MemoryExtractionProposal>,
    conversation: Vec<ConversationTurn>,
    jobs: Vec<Job>,
    message: String,
}

impl App {
    fn new() -> Result<Self, Box<dyn Error>> {
        let memory_root = memory_root()?;
        std::fs::create_dir_all(&memory_root)?;
        let db_path = db_path(&memory_root);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let qwen_url = env_or_default("ASSISTANT_QWEN_URL", "http://127.0.0.1:8080".to_string());
        let qwen_model =
            env_or_default("ASSISTANT_QWEN_MODEL", "Qwen3.5-4B-Q4_K_M.gguf".to_string());
        let embedding_url = env_or_default(
            "ASSISTANT_EMBEDDING_URL",
            "http://127.0.0.1:8081".to_string(),
        );
        let embedding_model = env_or_default(
            "ASSISTANT_EMBEDDING_MODEL",
            "bge-small-en-v1.5-q8_0.gguf".to_string(),
        );
        let background_workers_enabled = env::var("ASSISTANT_BACKGROUND_WORKERS")
            .map(|value| {
                !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no"
                )
            })
            .unwrap_or(true);

        let db = SqliteMemoryDb::open(&db_path)?;
        db.initialize_schema()?;
        let indexer = PersistentMemoryIndexer::new(
            &db_path,
            &memory_root,
            embedding_url.clone(),
            embedding_model.clone(),
        )?;

        let mut app = Self {
            db,
            indexer,
            memory_root,
            db_path,
            qwen_url,
            qwen_model,
            embedding_url,
            embedding_model,
            background_workers_enabled,
            tab: Tab::Overview,
            selected: 0,
            search_mode: false,
            search_query: String::new(),
            search_results: Vec::new(),
            tasks: Vec::new(),
            reminders: Vec::new(),
            proposals: Vec::new(),
            conversation: Vec::new(),
            jobs: Vec::new(),
            message: String::new(),
        };
        app.refresh()?;
        Ok(app)
    }

    fn refresh(&mut self) -> Result<(), Box<dyn Error>> {
        self.tasks = self.db.tasks(None, 200)?;
        self.reminders = self.db.reminders(None, 200)?;
        self.proposals = self.indexer.pending_memory_extraction_proposals(50)?;
        self.conversation = self.db.recent_conversation_turns_all(30)?;
        self.jobs = self.db.jobs(None, 100)?;
        if !self.search_query.trim().is_empty() {
            self.search_results = self.db.search(&self.search_query, 50)?;
        } else {
            self.search_results.clear();
        }
        let len = self.current_len();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len - 1);
        }
        Ok(())
    }

    fn current_len(&self) -> usize {
        match self.tab {
            Tab::Overview => 0,
            Tab::Tasks => self.tasks.len(),
            Tab::Reminders => self.reminders.len(),
            Tab::Proposals => self.proposals.len(),
            Tab::Conversation => self.conversation.len(),
            Tab::Jobs => self.jobs.len(),
            Tab::Search => self.search_results.len(),
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = 0;
        self.search_mode = false;
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
        match self.indexer.apply_memory_extraction_proposal(&id) {
            Ok(paths) if paths.is_empty() => {
                self.message = "Proposal was already applied.".to_string()
            }
            Ok(paths) => {
                self.message = format!("Applied proposal: {}", paths.join(", "));
                let _ = self.refresh();
            }
            Err(error) => self.message = format!("Apply failed: {error}"),
        }
    }

    fn reject_selected_proposal(&mut self) {
        if self.tab != Tab::Proposals || self.proposals.is_empty() {
            return;
        }
        let id = self.proposals[self.selected].id.clone();
        match self.indexer.reject_memory_extraction_proposal(&id) {
            Ok(true) => {
                self.message = "Proposal rejected.".to_string();
                let _ = self.refresh();
            }
            Ok(false) => self.message = "Proposal was already handled.".to_string(),
            Err(error) => self.message = format!("Reject failed: {error}"),
        }
    }

    fn run_search(&mut self) {
        self.search_mode = false;
        match self.db.search(&self.search_query, 50) {
            Ok(results) => {
                self.search_results = results;
                self.selected = 0;
                self.message = format!("{} search result(s).", self.search_results.len());
            }
            Err(error) => self.message = format!("Search failed: {error}"),
        }
    }

    fn handle_event(&mut self, event: Event) -> Result<bool, Box<dyn Error>> {
        let Event::Key(key) = event else {
            return Ok(true);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(true);
        }

        if self.search_mode {
            match key.code {
                KeyCode::Esc => self.search_mode = false,
                KeyCode::Enter => self.run_search(),
                KeyCode::Backspace => {
                    self.search_query.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.search_query.push(c)
                }
                _ => {}
            }
            return Ok(true);
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Tab => self.next_tab(),
            KeyCode::BackTab => self.previous_tab(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('r') => {
                self.refresh()?;
                self.message = "Refreshed.".to_string();
            }
            KeyCode::Char('a') => self.accept_selected_proposal(),
            KeyCode::Char('x') => self.reject_selected_proposal(),
            KeyCode::Char('/') if self.tab == Tab::Search => self.search_mode = true,
            KeyCode::Char(c @ '1'..='7') => {
                self.set_tab(Tab::from_index(c as usize - '1' as usize))
            }
            _ => {}
        }
        Ok(true)
    }

    fn title(&self) -> String {
        let title = TABS[self.tab.index()];
        if self.search_mode {
            format!("Search: {}_", self.search_query)
        } else if self.tab == Tab::Proposals {
            format!("{title}  [a approve | x reject]")
        } else {
            title.to_string()
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
                    .title("Assistant TUI"),
            ),
            vertical[0],
        );

        match self.tab {
            Tab::Overview => self.draw_overview(frame, vertical[1]),
            Tab::Tasks => self.draw_tasks(frame, vertical[1]),
            Tab::Reminders => self.draw_reminders(frame, vertical[1]),
            Tab::Proposals => self.draw_proposals(frame, vertical[1]),
            Tab::Conversation => self.draw_conversation(frame, vertical[1]),
            Tab::Jobs => self.draw_jobs(frame, vertical[1]),
            Tab::Search => self.draw_search(frame, vertical[1]),
        }

        let footer = if self.message.is_empty() {
            "q quit | tab switch | ↑↓ select | r refresh | / search | a/x proposal action"
        } else {
            &self.message
        };
        frame.render_widget(
            Paragraph::new(footer).block(Block::default().borders(Borders::ALL)),
            vertical[2],
        );
    }

    fn draw_overview(&self, frame: &mut Frame, area: Rect) {
        let queued = self
            .jobs
            .iter()
            .filter(|job| matches!(job.status, JobStatus::Queued))
            .count();
        let running = self
            .jobs
            .iter()
            .filter(|job| matches!(job.status, JobStatus::Running))
            .count();
        let text = vec![
            Line::from(format!("Memory root: {}", self.memory_root.display())),
            Line::from(format!("SQLite DB:   {}", self.db_path.display())),
            Line::from(format!(
                "Qwen:        {} ({})",
                self.qwen_model, self.qwen_url
            )),
            Line::from(format!(
                "Embeddings:  {} ({})",
                self.embedding_model, self.embedding_url
            )),
            Line::from(format!(
                "Workers:     {}",
                if self.background_workers_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )),
            Line::from(format!("Tasks:       {}", self.tasks.len())),
            Line::from(format!("Reminders:   {}", self.reminders.len())),
            Line::from(format!("Proposals:   {} pending", self.proposals.len())),
            Line::from(format!(
                "Jobs:        {} queued, {} running",
                queued, running
            )),
            Line::from(format!(
                "Conversation: {} recent turns",
                self.conversation.len()
            )),
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
            let summary = proposal_summary(proposal);
            ListItem::new(format!(
                "{}  [{}]  {}",
                proposal.id, proposal.decision, summary
            ))
        });
        self.draw_list(frame, area, items.collect(), &self.title());
    }

    fn draw_conversation(&self, frame: &mut Frame, area: Rect) {
        let items = self.conversation.iter().map(|turn| {
            let user = compact(&turn.user_text, 80);
            let assistant = compact(&turn.assistant_text, 100);
            ListItem::new(vec![
                Line::from(format!("{}  You: {}", turn.created_at, user)),
                Line::from(format!("        Assistant: {}", assistant)),
            ])
        });
        self.draw_list(frame, area, items.collect(), "Recent conversation");
    }

    fn draw_jobs(&self, frame: &mut Frame, area: Rect) {
        let items = self.jobs.iter().map(|job| {
            let status = match job.status {
                JobStatus::Queued => "queued",
                JobStatus::Running => "running",
                JobStatus::Completed => "completed",
                JobStatus::Failed => "failed",
                JobStatus::Cancelled => "cancelled",
            };
            ListItem::new(format!(
                "{}  [{}] {}  next={}",
                job.id, status, job.job_type, job.next_run_at
            ))
        });
        self.draw_list(frame, area, items.collect(), "Jobs");
    }

    fn draw_search(&self, frame: &mut Frame, area: Rect) {
        let mut items = Vec::new();
        if self.search_query.is_empty() {
            items.push(ListItem::new(
                "Press / to enter a search query, then Enter.",
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
        self.draw_list(
            frame,
            area,
            items,
            &format!("Search: {}", self.search_query),
        );
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

fn proposal_summary(proposal: &MemoryExtractionProposal) -> String {
    let value: serde_json::Value = match serde_json::from_str(&proposal.payload_json) {
        Ok(value) => value,
        Err(_) => return "invalid proposal JSON".to_string(),
    };
    let items = value
        .get("items")
        .and_then(|value| value.as_array())
        .map_or(0, Vec::len);
    format!("{} item(s), turn {}", items, proposal.conversation_turn_id)
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

fn run() -> Result<(), Box<dyn Error>> {
    let mut app = App::new()?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = loop {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(250))? && !app.handle_event(event::read()?)? {
            break Ok(());
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
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
        assert_eq!(Tab::from_index(6), Tab::Search);
        assert_eq!(Tab::from_index(7), Tab::Overview);
    }

    #[test]
    fn compact_collapses_and_truncates_text() {
        assert_eq!(compact("hello   world", 20), "hello world");
        assert_eq!(compact("abcdefghijklmnopqrstuvwxyz", 8), "abcdefg…");
    }
}
