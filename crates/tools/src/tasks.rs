use indexer::frontmatter::parse as parse_markdown;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlite_memory::{ReminderRecord, SqliteMemoryDb, TaskRecord};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use time::{
    Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset, Weekday,
    format_description::well_known::Rfc3339,
};

use crate::calendar_sync::ReminderCalendarSync;
use crate::{Result, Tool, ToolDefinition, ToolPermission, ToolResult};

const DEFAULT_LIST_LIMIT: usize = 20;
const MAX_LIST_LIMIT: usize = 100;
const MAX_QUERY_CHARS: usize = 256;
const MAX_TITLE_CHARS: usize = 256;
const MAX_BODY_CHARS: usize = 12_000;
const MAX_DUE_CHARS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Task,
    Reminder,
}

impl ItemKind {
    fn memory_kind(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Reminder => "reminder",
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::Task => "task:",
            Self::Reminder => "reminder:",
        }
    }

    fn note_directory(self) -> &'static str {
        match self {
            Self::Task => "journal/tasks",
            Self::Reminder => "journal/reminders",
        }
    }

    fn active_statuses(self) -> &'static [&'static str] {
        match self {
            Self::Task => &["open", "in_progress"],
            Self::Reminder => &["scheduled", "triggered"],
        }
    }

    fn valid_status(self, status: &str) -> bool {
        match self {
            Self::Task => matches!(
                status,
                "open" | "in_progress" | "completed" | "cancelled" | "expired" | "archived"
            ),
            Self::Reminder => matches!(
                status,
                "scheduled" | "triggered" | "completed" | "cancelled"
            ),
        }
    }

    fn status_name(self) -> &'static str {
        match self {
            Self::Task => "task_status",
            Self::Reminder => "reminder_status",
        }
    }

    fn list_statuses(self) -> &'static str {
        match self {
            Self::Task => "open, in_progress, completed, cancelled, expired, archived",
            Self::Reminder => "scheduled, triggered, completed, cancelled",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationArguments {
    operation: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_string")]
    due: Option<Option<String>>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    calendar_sync: Option<bool>,
}

fn default_limit() -> usize {
    DEFAULT_LIST_LIMIT
}

fn deserialize_optional_optional_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

fn validate_text(value: &str, field: &str, max_chars: usize, allow_empty: bool) -> Result<()> {
    if !allow_empty && value.trim().is_empty() {
        return Err(format!("{field} cannot be empty.").into());
    }
    if value.chars().count() > max_chars {
        return Err(format!("{field} exceeds the {max_chars}-character limit.").into());
    }
    if value.contains('\0') {
        return Err(format!("{field} cannot contain NUL bytes.").into());
    }
    if field != "body" && value.contains(['\n', '\r']) {
        return Err(format!("{field} must be a single line.").into());
    }
    Ok(())
}

fn local_offset() -> Result<UtcOffset> {
    UtcOffset::current_local_offset()
        .map_err(|error| format!("Could not determine the local timezone offset: {error}").into())
}

fn format_utc(value: OffsetDateTime) -> Result<String> {
    Ok(value.to_offset(UtcOffset::UTC).format(&Rfc3339)?)
}

fn format_local(value: &str) -> Result<String> {
    let utc = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|error| format!("Invalid stored UTC timestamp: {error}"))?;
    Ok(utc.to_offset(local_offset()?).format(&Rfc3339)?)
}

fn offset_display(offset: UtcOffset) -> String {
    let seconds = offset.whole_seconds();
    if seconds == 0 {
        return "+00:00".to_string();
    }
    let sign = if seconds < 0 { '-' } else { '+' };
    let absolute = seconds.unsigned_abs();
    let hours = absolute / 3600;
    let minutes = (absolute % 3600) / 60;
    format!("{sign}{hours:02}:{minutes:02}")
}

fn month_from_number(value: u8) -> Result<Month> {
    match value {
        1 => Ok(Month::January),
        2 => Ok(Month::February),
        3 => Ok(Month::March),
        4 => Ok(Month::April),
        5 => Ok(Month::May),
        6 => Ok(Month::June),
        7 => Ok(Month::July),
        8 => Ok(Month::August),
        9 => Ok(Month::September),
        10 => Ok(Month::October),
        11 => Ok(Month::November),
        12 => Ok(Month::December),
        _ => Err("Month must be between 1 and 12.".into()),
    }
}

fn parse_date(value: &str) -> Result<Date> {
    let separator = if value.contains('/') { '/' } else { '-' };
    let parts = value.split(separator).collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err("Date must use DD/MM/YYYY, DD-MM-YY, DD-MM-YYYY, or YYYY-MM-DD.".into());
    }

    let (year, month_number, day) = if separator == '/' {
        let day: u8 = parts[0].parse().map_err(|_| "Date day is invalid.")?;
        let month_number: u8 = parts[1].parse().map_err(|_| "Date month is invalid.")?;
        let year: i32 = parts[2].parse().map_err(|_| "Date year is invalid.")?;
        (year, month_number, day)
    } else if parts[0].len() == 4 {
        let year: i32 = parts[0].parse().map_err(|_| "Date year is invalid.")?;
        let month_number: u8 = parts[1].parse().map_err(|_| "Date month is invalid.")?;
        let day: u8 = parts[2].parse().map_err(|_| "Date day is invalid.")?;
        (year, month_number, day)
    } else {
        let day: u8 = parts[0].parse().map_err(|_| "Date day is invalid.")?;
        let month_number: u8 = parts[1].parse().map_err(|_| "Date month is invalid.")?;
        let raw_year: i32 = parts[2].parse().map_err(|_| "Date year is invalid.")?;
        let year = if parts[2].len() == 2 {
            2000 + raw_year
        } else if parts[2].len() == 4 {
            raw_year
        } else {
            return Err("Date year must use YY or YYYY.".into());
        };
        (year, month_number, day)
    };

    Ok(Date::from_calendar_date(
        year,
        month_from_number(month_number)?,
        day,
    )?)
}

fn parse_clock(value: &str) -> Result<Time> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "midnight" => return Ok(Time::MIDNIGHT),
        "noon" => return Ok(Time::from_hms(12, 0, 0)?),
        "morning" => return Ok(Time::from_hms(9, 0, 0)?),
        "afternoon" => return Ok(Time::from_hms(15, 0, 0)?),
        "evening" => return Ok(Time::from_hms(18, 0, 0)?),
        "night" | "tonight" => return Ok(Time::from_hms(21, 0, 0)?),
        _ => {}
    }

    let mut clock = normalized.as_str();
    let mut period = None;
    for suffix in ["am", "pm"] {
        if let Some(stripped) = clock.strip_suffix(suffix) {
            clock = stripped.trim();
            period = Some(suffix);
            break;
        }
    }

    let parts = clock.split(':').collect::<Vec<_>>();
    if parts.len() > 2 || parts.first().is_none_or(|part| part.is_empty()) {
        return Err("Time must use H or H:MM.".into());
    }

    let mut hour: u8 = parts[0].parse().map_err(|_| "Time hour is invalid.")?;
    let minute: u8 = parts.get(1).map_or(Ok(0), |value| {
        value.parse().map_err(|_| "Time minute is invalid.")
    })?;

    if minute > 59 {
        return Err("Time minute must be between 0 and 59.".into());
    }

    if let Some(period) = period {
        if hour == 0 || hour > 12 {
            return Err("12-hour time must use an hour from 1 to 12.".into());
        }
        if period == "am" {
            if hour == 12 {
                hour = 0;
            }
        } else if hour != 12 {
            hour += 12;
        }
    } else if hour > 23 {
        return Err("24-hour time must use an hour from 0 to 23.".into());
    }

    Ok(Time::from_hms(hour, minute, 0)?)
}

fn next_weekday(date: Date, target: Weekday, include_today: bool) -> Result<Date> {
    let mut cursor = date;
    for day_offset in 0..8 {
        if (include_today || day_offset > 0) && cursor.weekday() == target {
            return Ok(cursor);
        }
        cursor = cursor
            .next_day()
            .ok_or("Could not calculate the next calendar day.")?;
    }
    Err("Could not resolve weekday.".into())
}

fn parse_weekday(value: &str) -> Option<Weekday> {
    match value {
        "monday" | "mon" => Some(Weekday::Monday),
        "tuesday" | "tue" | "tues" => Some(Weekday::Tuesday),
        "wednesday" | "wed" => Some(Weekday::Wednesday),
        "thursday" | "thu" | "thur" | "thurs" => Some(Weekday::Thursday),
        "friday" | "fri" => Some(Weekday::Friday),
        "saturday" | "sat" => Some(Weekday::Saturday),
        "sunday" | "sun" => Some(Weekday::Sunday),
        _ => None,
    }
}

fn split_date_and_time(value: &str) -> (String, Option<String>) {
    let normalized = value.trim().to_ascii_lowercase().replace(',', "");

    if let Some(time) = normalized.strip_prefix("at ") {
        if parse_clock(time.trim()).is_ok() {
            return ("today".to_string(), Some(time.trim().to_string()));
        }
    }

    if let Some((date, time)) = normalized.split_once(" at ") {
        return (date.trim().to_string(), Some(time.trim().to_string()));
    }

    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.len() >= 2 {
        let two = format!("{} {}", tokens[tokens.len() - 2], tokens[tokens.len() - 1]);
        if parse_clock(&two).is_ok() {
            return (tokens[..tokens.len() - 2].join(" "), Some(two));
        }
    }
    if let Some(last) = tokens.last() {
        if parse_clock(last).is_ok() && tokens.len() > 1 {
            return (
                tokens[..tokens.len() - 1].join(" "),
                Some((*last).to_string()),
            );
        }
    }

    if normalized == "tonight" {
        return ("today".to_string(), Some("tonight".to_string()));
    }

    (normalized, None)
}

fn parse_relative_duration(value: &str, now_utc: OffsetDateTime) -> Result<Option<OffsetDateTime>> {
    let normalized = value.trim().to_ascii_lowercase();
    let Some(rest) = normalized.strip_prefix("in ") else {
        return Ok(None);
    };

    let parts = rest.split_whitespace().collect::<Vec<_>>();

    if parts.as_slice() == ["half", "an", "hour"] {
        return Ok(Some(now_utc.checked_add(Duration::minutes(30)).ok_or(
            "Relative due time overflowed the supported date range.",
        )?));
    }

    if parts.len() != 2 {
        return Err("Relative due times must look like 'in 2 hours' or 'in half an hour'.".into());
    }

    let amount: i64 = match parts[0] {
        "a" | "an" => 1,
        other => other
            .parse()
            .map_err(|_| "Relative due time amount is invalid.")?,
    };
    if amount <= 0 {
        return Err("Relative due time amount must be positive.".into());
    }

    let duration = match parts[1].trim_end_matches('s') {
        "minute" => Duration::minutes(amount),
        "hour" => Duration::hours(amount),
        "day" => Duration::days(amount),
        "week" => Duration::weeks(amount),
        _ => {
            return Err("Supported relative due units are minutes, hours, days, and weeks.".into());
        }
    };

    Ok(Some(now_utc.checked_add(duration).ok_or(
        "Relative due time overflowed the supported date range.",
    )?))
}

fn resolve_local_date(expression: &str, today: Date) -> Result<Date> {
    let expression = expression.trim();

    match expression {
        "today" | "this day" | "this" => return Ok(today),
        "tomorrow" => {
            return today
                .next_day()
                .ok_or_else(|| "Could not calculate tomorrow.".into());
        }
        "yesterday" => {
            return today
                .previous_day()
                .ok_or_else(|| "Could not calculate yesterday.".into());
        }
        _ => {}
    }

    if let Some(weekday) = parse_weekday(expression) {
        return next_weekday(today, weekday, true);
    }

    if let Some(rest) = expression.strip_prefix("next ") {
        let weekday = parse_weekday(rest.trim()).ok_or("Unsupported weekday expression.")?;
        return next_weekday(today, weekday, false);
    }

    parse_date(expression)
}

fn local_datetime_to_utc(date: Date, time: Time) -> Result<OffsetDateTime> {
    let offset = local_offset()?;
    let local = PrimitiveDateTime::new(date, time).assume_offset(offset);
    Ok(local.to_offset(UtcOffset::UTC))
}

/// Returns whether a string is a valid natural-language due expression under the
/// task/reminder temporal policy.
pub fn is_due_expression(value: &str) -> bool {
    parse_due_expression(value, OffsetDateTime::now_utc()).is_ok()
}

pub fn parse_due_expression(value: &str, now_utc: OffsetDateTime) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Due time cannot be empty.".into());
    }
    if value.chars().count() > MAX_DUE_CHARS {
        return Err("Due time expression is too long.".into());
    }

    if let Ok(timestamp) = OffsetDateTime::parse(value, &Rfc3339) {
        return format_utc(timestamp);
    }

    if let Some(timestamp) = parse_relative_duration(value, now_utc)? {
        return format_utc(timestamp);
    }

    let local_now = now_utc.to_offset(local_offset()?);
    let (mut date_expression, time_expression) = split_date_and_time(value);
    if date_expression == "this" && time_expression.is_some() {
        date_expression = "today".to_string();
    }

    let date = resolve_local_date(&date_expression, local_now.date())?;
    // Date-only expressions resolve to the user's local 09:00 by policy.
    let time = time_expression
        .as_deref()
        .map(parse_clock)
        .transpose()?
        .unwrap_or(Time::from_hms(9, 0, 0)?);

    format_utc(local_datetime_to_utc(date, time)?)
}

#[derive(Debug, Clone, Copy)]
enum DueFilter {
    Overdue,
    Upcoming,
    Range(Date, Date),
}

fn parse_due_filter(value: &str, now_utc: OffsetDateTime) -> Result<DueFilter> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized == "overdue" {
        return Ok(DueFilter::Overdue);
    }
    if normalized == "upcoming" || normalized == "future" {
        return Ok(DueFilter::Upcoming);
    }

    let local_now = now_utc.to_offset(local_offset()?);
    let today = local_now.date();
    let date = match normalized.as_str() {
        "today" => today,
        "tomorrow" => today.next_day().ok_or("Could not calculate tomorrow.")?,
        expression => resolve_local_date(expression, today)?,
    };

    let end = date
        .next_day()
        .ok_or("Could not calculate the end of the requested day.")?;

    Ok(DueFilter::Range(date, end))
}

fn parse_stored_due(value: &str) -> Result<OffsetDateTime> {
    Ok(OffsetDateTime::parse(value, &Rfc3339)?)
}

fn matches_due_filter(
    value: Option<&str>,
    filter: DueFilter,
    now_utc: OffsetDateTime,
) -> Result<bool> {
    let Some(value) = value else {
        return Ok(false);
    };
    let due = parse_stored_due(value)?;
    match filter {
        DueFilter::Overdue => Ok(due < now_utc),
        DueFilter::Upcoming => Ok(due > now_utc),
        DueFilter::Range(start, end) => {
            let start_utc = local_datetime_to_utc(start, Time::MIDNIGHT)?;
            let end_utc = local_datetime_to_utc(end, Time::MIDNIGHT)?;
            Ok(due >= start_utc && due < end_utc)
        }
    }
}

fn stable_note_id(kind: ItemKind, title: &str, body: &str, due_at: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.memory_kind().as_bytes());
    hasher.update(b"\n");
    hasher.update(title.trim().as_bytes());
    hasher.update(b"\n");
    hasher.update(body.as_bytes());
    hasher.update(b"\n");
    hasher.update(due_at.unwrap_or("").as_bytes());
    let digest = hasher.finalize();
    format!(
        "{}-{}",
        kind.memory_kind(),
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn yaml_scalar(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

fn render_new_note(
    kind: ItemKind,
    note_id: &str,
    title: &str,
    body: &str,
    status: &str,
    due_at: Option<&str>,
    created_at: &str,
    calendar_sync_enabled: bool,
    calendar_id: Option<&str>,
) -> String {
    let mut output = String::new();
    output.push_str("---\n");
    output.push_str("id: ");
    output.push_str(&yaml_scalar(note_id));
    output.push('\n');
    output.push_str("title: ");
    output.push_str(&yaml_scalar(title.trim()));
    output.push('\n');
    output.push_str("type: \"journal\"\n");
    output.push_str("namespace: \"global\"\n");
    output.push_str("status: \"active\"\n");
    output.push_str("memory_kind: ");
    output.push_str(&yaml_scalar(kind.memory_kind()));
    output.push('\n');
    output.push_str(kind.status_name());
    output.push_str(": ");
    output.push_str(&yaml_scalar(status));
    output.push('\n');
    if let Some(due_at) = due_at {
        output.push_str("due_at: ");
        output.push_str(&yaml_scalar(due_at));
        output.push('\n');
    }
    if kind == ItemKind::Reminder {
        output.push_str("calendar_sync_enabled: ");
        output.push_str(if calendar_sync_enabled {
            "\"true\""
        } else {
            "\"false\""
        });
        output.push('\n');
        if calendar_sync_enabled {
            if let Some(calendar_id) = calendar_id {
                output.push_str("google_calendar_id: ");
                output.push_str(&yaml_scalar(calendar_id));
                output.push('\n');
            }
            output.push_str("calendar_sync_status: \"pending\"\n");
        } else {
            output.push_str("calendar_sync_status: \"disabled\"\n");
        }
    }
    output.push_str("created_at: ");
    output.push_str(&yaml_scalar(created_at));
    output.push('\n');
    output.push_str("updated_at: ");
    output.push_str(&yaml_scalar(created_at));
    output.push_str("\n---\n\n");
    output.push_str(body);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn now_timestamp() -> Result<String> {
    format_utc(OffsetDateTime::now_utc())
}

fn path_for(root: &Path, kind: ItemKind, note_id: &str) -> PathBuf {
    root.join(kind.note_directory())
        .join(format!("{note_id}.md"))
}

fn ensure_safe_existing_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        || relative_path.extension() != Some(std::ffi::OsStr::new("md"))
    {
        return Err("Task/reminder note path must be a relative Markdown file inside the configured memory root.".into());
    }

    let canonical_root = fs::canonicalize(root)?;
    let canonical_path = fs::canonicalize(root.join(relative_path))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err("Task/reminder note path escapes the configured memory root.".into());
    }
    if !canonical_path.is_file() {
        return Err("Task/reminder source path is not a regular file.".into());
    }
    Ok(canonical_path)
}

fn atomic_replace(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or("Task/reminder note has no parent directory.")?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("task-reminder.md");
    let temp_path = parent.join(format!(".{filename}.tmp-{}-{unique}", std::process::id()));

    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    result.map_err(|error| error.into())
}

fn body_matches_requested(existing: &str, requested: &str) -> bool {
    existing == requested || (!requested.ends_with('\n') && existing == format!("{requested}\n"))
}

fn create_new_file(path: &Path, content: &str) -> Result<bool> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(true)
}

fn frontmatter_body_start(content: &str) -> Result<usize> {
    let mut offset = 0usize;
    let mut lines = content.split_inclusive('\n');
    let first = lines.next().ok_or("Task/reminder note is empty.")?;
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Err("Task/reminder mutations require Markdown frontmatter.".into());
    }
    offset += first.len();

    for line in lines {
        if line.trim_end_matches(['\r', '\n']).trim() == "---" {
            return Ok(offset + line.len());
        }
        offset += line.len();
    }

    if content.trim_end().ends_with("---") {
        let marker = content
            .rfind("---")
            .ok_or("Task/reminder frontmatter is malformed.")?;
        return Ok(marker + 3);
    }

    Err("Task/reminder mutations require a closed frontmatter block.".into())
}

fn set_top_level_field(content: &str, key: &str, value: Option<&str>) -> Result<String> {
    let mut lines = content.lines().map(str::to_string).collect::<Vec<_>>();
    if lines.first().map(String::as_str) != Some("---") {
        return Err("Task/reminder mutations require Markdown frontmatter.".into());
    }

    let end = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (line.trim() == "---").then_some(index))
        .ok_or("Task/reminder mutations require a closed frontmatter block.")?;

    let replacement = value.map(|value| format!("{key}: {}", yaml_scalar(value)));
    let mut found = false;

    for index in 1..end {
        if lines[index] != lines[index].trim_start() {
            continue;
        }
        let Some((name, _)) = lines[index].split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        found = true;
        if let Some(replacement) = &replacement {
            lines[index] = replacement.clone();
        } else {
            lines.remove(index);
        }
        break;
    }

    if !found {
        if let Some(replacement) = replacement {
            lines.insert(end, replacement);
        }
    }

    let had_newline = content.ends_with('\n');
    let mut result = lines.join("\n");
    if had_newline {
        result.push('\n');
    }
    Ok(result)
}

fn mutate_markdown(
    kind: ItemKind,
    path: &Path,
    expected_note_id: &str,
    expected_title: &str,
    expected_status: &str,
    expected_due_at: Option<&str>,
    expected_updated_at: Option<&str>,
    new_title: Option<&str>,
    new_body: Option<&str>,
    new_status: &str,
    new_due_at: Option<Option<&str>>,
) -> Result<()> {
    let original = fs::read_to_string(path)?;
    let parsed = parse_markdown(&original);

    if parsed.metadata.id.as_deref() != Some(expected_note_id)
        || parsed.metadata.memory_kind.as_deref() != Some(kind.memory_kind())
    {
        return Err("Task/reminder note changed since it was indexed; refresh and retry.".into());
    }
    if parsed.metadata.status.as_deref().unwrap_or("active") != "active" {
        return Err("Task/reminder note is not active.".into());
    }

    let actual_status = parsed
        .metadata
        .task_status
        .as_deref()
        .or(parsed.metadata.reminder_status.as_deref())
        .unwrap_or(expected_status);
    if actual_status != expected_status {
        return Err("Task/reminder status changed since it was indexed; refresh and retry.".into());
    }
    if parsed.metadata.due_at.as_deref() != expected_due_at {
        return Err(
            "Task/reminder due time changed since it was indexed; refresh and retry.".into(),
        );
    }
    if parsed.metadata.title.as_deref().unwrap_or("") != expected_title {
        return Err("Task/reminder title changed since it was indexed; refresh and retry.".into());
    }
    if expected_updated_at.is_some() && parsed.metadata.updated_at.as_deref() != expected_updated_at
    {
        return Err("Task/reminder note was updated after indexing; refresh and retry.".into());
    }

    let mut content = original.clone();
    if let Some(title) = new_title {
        content = set_top_level_field(&content, "title", Some(title))?;
    }
    if let Some(body) = new_body {
        let body_start = frontmatter_body_start(&content)?;
        let mut replacement = body.to_string();
        if !replacement.is_empty() && !replacement.ends_with('\n') {
            replacement.push('\n');
        }
        content = format!("{}{}", &content[..body_start], replacement);
    }

    content = set_top_level_field(&content, kind.status_name(), Some(new_status))?;
    content = set_top_level_field(&content, "updated_at", Some(&now_timestamp()?))?;
    if let Some(due) = new_due_at {
        content = set_top_level_field(&content, "due_at", due)?;
    }

    if content == original {
        return Ok(());
    }

    atomic_replace(path, &content)
}

#[derive(Debug, Clone, Default)]
struct CalendarSyncMetadata {
    enabled: bool,
    calendar_id: Option<String>,
    event_id: Option<String>,
    status: Option<String>,
    last_synced_at: Option<String>,
    error: Option<String>,
}

fn read_calendar_sync_metadata(path: &Path) -> Result<CalendarSyncMetadata> {
    let parsed = parse_markdown(&fs::read_to_string(path)?);
    Ok(CalendarSyncMetadata {
        enabled: parsed.metadata.calendar_sync_enabled.unwrap_or(false),
        calendar_id: parsed.metadata.google_calendar_id,
        event_id: parsed.metadata.google_event_id,
        status: parsed.metadata.calendar_sync_status,
        last_synced_at: parsed.metadata.calendar_last_synced_at,
        error: parsed.metadata.calendar_sync_error,
    })
}

fn persist_calendar_sync_metadata(path: &Path, metadata: &CalendarSyncMetadata) -> Result<()> {
    let original = fs::read_to_string(path)?;
    let mut content = original.clone();
    content = set_top_level_field(
        &content,
        "calendar_sync_enabled",
        Some(if metadata.enabled { "true" } else { "false" }),
    )?;
    content = set_top_level_field(
        &content,
        "google_calendar_id",
        metadata.calendar_id.as_deref(),
    )?;
    content = set_top_level_field(&content, "google_event_id", metadata.event_id.as_deref())?;
    content = set_top_level_field(&content, "calendar_sync_status", metadata.status.as_deref())?;
    content = set_top_level_field(
        &content,
        "calendar_last_synced_at",
        metadata.last_synced_at.as_deref(),
    )?;
    content = set_top_level_field(&content, "calendar_sync_error", metadata.error.as_deref())?;

    if content != original {
        atomic_replace(path, &content)?;
    }
    Ok(())
}

fn record_calendar_sync_failure(
    path: &Path,
    metadata: &CalendarSyncMetadata,
    error: &str,
) -> Result<()> {
    let mut next = metadata.clone();
    next.status = Some("error".to_string());
    next.error = Some(error.to_string());
    persist_calendar_sync_metadata(path, &next)
}

fn sync_created_reminder(
    path: &Path,
    sync: &ReminderCalendarSync,
    title: &str,
    body: &str,
    due_at: Option<&str>,
) -> Result<()> {
    let mut metadata = read_calendar_sync_metadata(path)?;
    if !metadata.enabled {
        return Ok(());
    }

    let Some(due_at) = due_at else {
        metadata.status = Some("skipped".to_string());
        metadata.error =
            Some("Reminder has no due time; no Calendar event was created.".to_string());
        persist_calendar_sync_metadata(path, &metadata)?;
        return Ok(());
    };

    let calendar_id = metadata
        .calendar_id
        .clone()
        .unwrap_or_else(|| sync.calendar_id().to_string());
    metadata.calendar_id = Some(calendar_id.clone());
    metadata.status = Some("pending".to_string());
    metadata.error = None;
    persist_calendar_sync_metadata(path, &metadata)?;

    match sync.create_event(&calendar_id, title, body, due_at) {
        Ok((calendar_id, event_id)) => {
            metadata.calendar_id = Some(calendar_id);
            metadata.event_id = Some(event_id);
            metadata.status = Some("synced".to_string());
            metadata.last_synced_at = Some(now_timestamp()?);
            metadata.error = None;
            persist_calendar_sync_metadata(path, &metadata)
        }
        Err(error) => record_calendar_sync_failure(path, &metadata, &error.to_string()),
    }
}

fn sync_updated_reminder(
    path: &Path,
    sync: &ReminderCalendarSync,
    title: &str,
    body: &str,
    due_at: Option<&str>,
) -> Result<()> {
    let mut metadata = read_calendar_sync_metadata(path)?;
    if !metadata.enabled {
        return Ok(());
    }

    let calendar_id = metadata
        .calendar_id
        .clone()
        .unwrap_or_else(|| sync.calendar_id().to_string());
    metadata.calendar_id = Some(calendar_id.clone());

    match (metadata.event_id.as_deref(), due_at) {
        (Some(event_id), Some(due_at)) => {
            match sync.update_event(&calendar_id, event_id, title, body, due_at) {
                Ok(()) => {
                    metadata.status = Some("synced".to_string());
                    metadata.last_synced_at = Some(now_timestamp()?);
                    metadata.error = None;
                    persist_calendar_sync_metadata(path, &metadata)
                }
                Err(error) => record_calendar_sync_failure(path, &metadata, &error.to_string()),
            }
        }
        (Some(_event_id), None) => {
            metadata.status = Some("preserved".to_string());
            metadata.last_synced_at = Some(now_timestamp()?);
            metadata.error = None;
            persist_calendar_sync_metadata(path, &metadata)
        }
        (None, Some(due_at)) => match sync.create_event(&calendar_id, title, body, due_at) {
            Ok((_calendar_id, event_id)) => {
                metadata.event_id = Some(event_id);
                metadata.status = Some("synced".to_string());
                metadata.last_synced_at = Some(now_timestamp()?);
                metadata.error = None;
                persist_calendar_sync_metadata(path, &metadata)
            }
            Err(error) => record_calendar_sync_failure(path, &metadata, &error.to_string()),
        },
        (None, None) => {
            metadata.status = Some("skipped".to_string());
            metadata.error =
                Some("Reminder has no due time; no Calendar event was created.".to_string());
            persist_calendar_sync_metadata(path, &metadata)
        }
    }
}

fn sync_cancelled_reminder(path: &Path) -> Result<()> {
    let mut metadata = read_calendar_sync_metadata(path)?;
    if !metadata.enabled {
        return Ok(());
    }

    if metadata.event_id.is_some() {
        metadata.status = Some("preserved".to_string());
        metadata.last_synced_at = Some(now_timestamp()?);
    } else {
        metadata.status = Some("skipped".to_string());
    }
    metadata.error = None;
    persist_calendar_sync_metadata(path, &metadata)
}

fn transition_allowed(kind: ItemKind, from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    match kind {
        ItemKind::Task => match from {
            "open" => matches!(
                to,
                "in_progress" | "completed" | "cancelled" | "expired" | "archived"
            ),
            "in_progress" => matches!(
                to,
                "open" | "completed" | "cancelled" | "expired" | "archived"
            ),
            "completed" | "cancelled" | "expired" => to == "archived",
            "archived" => false,
            _ => false,
        },
        ItemKind::Reminder => match from {
            "scheduled" => matches!(to, "triggered" | "completed" | "cancelled"),
            "triggered" => matches!(to, "completed" | "cancelled"),
            "completed" | "cancelled" => false,
            _ => false,
        },
    }
}

fn public_id(kind: ItemKind, id: &str) -> String {
    if id.starts_with(kind.id_prefix()) {
        id.to_string()
    } else {
        format!("{}{}", kind.id_prefix(), id)
    }
}

fn record_to_value(
    kind: ItemKind,
    db: &SqliteMemoryDb,
    id: &str,
    title: &str,
    body: &str,
    status: &str,
    due_at: Option<&str>,
    created_at: Option<&str>,
    updated_at: Option<&str>,
    note_id: &str,
) -> Result<Value> {
    Ok(serde_json::json!({
        "id": id,
        "note_id": note_id,
        "title": title,
        "body": body,
        "status": status,
        "due_at": due_at,
        "due_at_local": due_at.map(format_local).transpose()?,
        "created_at": created_at,
        "updated_at": updated_at,
        "path": db.note_path(note_id)?,
        "memory_kind": kind.memory_kind(),
        "timezone_offset": offset_display(local_offset()?),
    }))
}

fn task_to_value(db: &SqliteMemoryDb, task: &TaskRecord) -> Result<Value> {
    record_to_value(
        ItemKind::Task,
        db,
        &task.id,
        &task.title,
        &task.body,
        &task.status,
        task.due_at.as_deref(),
        task.created_at.as_deref(),
        task.updated_at.as_deref(),
        &task.note_id,
    )
}

fn reminder_to_value(db: &SqliteMemoryDb, reminder: &ReminderRecord) -> Result<Value> {
    record_to_value(
        ItemKind::Reminder,
        db,
        &reminder.id,
        &reminder.title,
        &reminder.body,
        &reminder.status,
        reminder.due_at.as_deref(),
        reminder.created_at.as_deref(),
        reminder.updated_at.as_deref(),
        &reminder.note_id,
    )
}

fn list_items(db: &SqliteMemoryDb, kind: ItemKind, arguments: ListArguments) -> Result<ToolResult> {
    if arguments.limit == 0 || arguments.limit > MAX_LIST_LIMIT {
        return Err(format!("limit must be 1..={MAX_LIST_LIMIT}.").into());
    }
    if let Some(query) = &arguments.query {
        validate_text(query, "query", MAX_QUERY_CHARS, false)?;
    }
    if let Some(id) = &arguments.id {
        validate_text(id, "id", 128, false)?;
    }
    if let Some(status) = &arguments.status {
        if status != "all" && !kind.valid_status(status) {
            return Err(format!(
                "Invalid {} status. Supported statuses: {}",
                kind.memory_kind(),
                kind.list_statuses()
            )
            .into());
        }
    }

    let now = OffsetDateTime::now_utc();
    let due_filter = arguments
        .due
        .as_deref()
        .map(|value| parse_due_filter(value, now))
        .transpose()?;

    let explicit_id = arguments.id.as_deref().map(|id| public_id(kind, id));
    let mut values = Vec::new();

    match kind {
        ItemKind::Task => {
            let records = if let Some(id) = explicit_id.as_deref() {
                db.task(id)?.into_iter().collect::<Vec<_>>()
            } else {
                db.tasks(
                    arguments
                        .status
                        .as_deref()
                        .filter(|status| *status != "all"),
                    MAX_LIST_LIMIT * 10,
                )?
            };

            for record in records {
                if explicit_id.as_deref().is_some_and(|id| id != record.id) {
                    continue;
                }
                if arguments.status.is_none()
                    && explicit_id.is_none()
                    && !kind.active_statuses().contains(&record.status.as_str())
                {
                    continue;
                }
                if let Some(query) = &arguments.query {
                    if !query_matches_text(&record.title, query)
                        && !query_matches_text(&record.body, query)
                    {
                        continue;
                    }
                }
                if let Some(filter) = due_filter {
                    if !matches_due_filter(record.due_at.as_deref(), filter, now)? {
                        continue;
                    }
                }
                values.push(task_to_value(db, &record)?);
                if values.len() >= arguments.limit {
                    break;
                }
            }
        }
        ItemKind::Reminder => {
            let records = if let Some(id) = explicit_id.as_deref() {
                db.reminder(id)?.into_iter().collect::<Vec<_>>()
            } else {
                db.reminders(
                    arguments
                        .status
                        .as_deref()
                        .filter(|status| *status != "all"),
                    MAX_LIST_LIMIT * 10,
                )?
            };

            for record in records {
                if explicit_id.as_deref().is_some_and(|id| id != record.id) {
                    continue;
                }
                if arguments.status.is_none()
                    && explicit_id.is_none()
                    && !kind.active_statuses().contains(&record.status.as_str())
                {
                    continue;
                }
                if let Some(query) = &arguments.query {
                    if !query_matches_text(&record.title, query)
                        && !query_matches_text(&record.body, query)
                    {
                        continue;
                    }
                }
                if let Some(filter) = due_filter {
                    if !matches_due_filter(record.due_at.as_deref(), filter, now)? {
                        continue;
                    }
                }
                values.push(reminder_to_value(db, &record)?);
                if values.len() >= arguments.limit {
                    break;
                }
            }
        }
    }

    Ok(ToolResult {
        success: true,
        output: serde_json::json!({
            "memory_kind": kind.memory_kind(),
            "count": values.len(),
            "items": values,
            "timezone_offset": offset_display(local_offset()?),
        }),
    })
}

fn query_matches_text(text: &str, query: &str) -> bool {
    let text_lower = text.to_ascii_lowercase();
    let query_lower = query.to_ascii_lowercase();

    if text_lower.contains(query_lower.trim()) {
        return true;
    }

    let text_tokens = text_lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let query_tokens = query_lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();

    if query_tokens.is_empty() {
        return false;
    }

    query_tokens.iter().all(|query_token| {
        text_tokens.iter().any(|text_token| {
            *text_token == *query_token
                || (query_token.len() >= 4
                    && text_token.len() >= 4
                    && (text_token.starts_with(query_token) || query_token.starts_with(text_token)))
        })
    })
}

fn validate_create(kind: ItemKind, arguments: &MutationArguments) -> Result<()> {
    let title = arguments.title.as_deref().ok_or("Create requires title.")?;
    validate_text(title, "title", MAX_TITLE_CHARS, false)?;
    if let Some(body) = &arguments.body {
        validate_text(body, "body", MAX_BODY_CHARS, true)?;
    }
    if let Some(status) = &arguments.status {
        if !kind.valid_status(status) {
            return Err(format!("Invalid {} status: {status}.", kind.memory_kind()).into());
        }
        let initial = match kind {
            ItemKind::Task => "open",
            ItemKind::Reminder => "scheduled",
        };
        if status != initial {
            return Err(format!(
                "New {} items must start in '{initial}' status.",
                kind.memory_kind()
            )
            .into());
        }
    }
    Ok(())
}

fn validate_mutation_shape(kind: ItemKind, arguments: &MutationArguments) -> Result<()> {
    let operation = arguments.operation.trim().to_ascii_lowercase();
    let allowed_operations = match kind {
        ItemKind::Task => "create, update, complete, cancel",
        ItemKind::Reminder => "create, update, complete, cancel, trigger",
    };
    if !matches!(
        operation.as_str(),
        "create" | "update" | "complete" | "cancel" | "trigger"
    ) || (operation == "trigger" && kind != ItemKind::Reminder)
    {
        return Err(format!(
            "Unsupported {} operation '{operation}'. Supported operations: {allowed_operations}.",
            kind.memory_kind()
        )
        .into());
    }

    if let Some(id) = &arguments.id {
        validate_text(id, "id", 128, false)?;
    }
    if let Some(title) = &arguments.title {
        validate_text(title, "title", MAX_TITLE_CHARS, false)?;
    }
    if let Some(body) = &arguments.body {
        validate_text(body, "body", MAX_BODY_CHARS, true)?;
    }
    if let Some(due) = &arguments.due {
        if let Some(due) = due {
            validate_text(due, "due", MAX_DUE_CHARS, false)?;
        }
    }
    if let Some(status) = &arguments.status {
        if !kind.valid_status(status) {
            return Err(format!("Invalid {} status: {status}.", kind.memory_kind()).into());
        }
    }

    if operation != "create" && arguments.id.as_deref().is_none() {
        return Err(format!("{operation} requires an exact item id.").into());
    }

    if let Some(calendar_sync) = arguments.calendar_sync {
        if kind != ItemKind::Reminder {
            return Err("calendar_sync is only supported for reminders.".into());
        }
        let _ = calendar_sync;
    }

    if matches!(operation.as_str(), "complete" | "cancel" | "trigger")
        && (arguments.title.is_some()
            || arguments.body.is_some()
            || arguments.due.is_some()
            || arguments.status.is_some()
            || arguments.calendar_sync.is_some())
    {
        return Err(format!("{operation} only accepts operation and id.").into());
    }

    if operation == "update"
        && arguments.title.is_none()
        && arguments.body.is_none()
        && arguments.due.is_none()
        && arguments.status.is_none()
        && arguments.calendar_sync.is_none()
    {
        return Err(
            "update requires at least one of title, body, due, status, or calendar_sync.".into(),
        );
    }

    Ok(())
}

fn mutate_item(
    root: &Path,
    db: &SqliteMemoryDb,
    kind: ItemKind,
    arguments: MutationArguments,
    calendar_sync: Option<&ReminderCalendarSync>,
) -> Result<ToolResult> {
    validate_mutation_shape(kind, &arguments)?;
    let operation = arguments.operation.trim().to_ascii_lowercase();

    if operation == "create" {
        validate_create(kind, &arguments)?;
        let title = arguments.title.as_deref().ok_or("Create requires title.")?;
        let body = arguments.body.as_deref().unwrap_or("");
        let due_at = arguments
            .due
            .as_ref()
            .and_then(|value| value.as_deref())
            .map(|value| parse_due_expression(value, OffsetDateTime::now_utc()))
            .transpose()?;
        let status = arguments.status.as_deref().unwrap_or_else(|| match kind {
            ItemKind::Task => "open",
            ItemKind::Reminder => "scheduled",
        });
        let note_id = stable_note_id(kind, title, body, due_at.as_deref());
        let path = path_for(root, kind, &note_id);
        fs::create_dir_all(path.parent().ok_or("Task/reminder path has no parent.")?)?;
        let created_at = now_timestamp()?;
        let calendar_sync_enabled =
            kind == ItemKind::Reminder && arguments.calendar_sync.unwrap_or(false);
        let content = render_new_note(
            kind,
            &note_id,
            title,
            body,
            status,
            due_at.as_deref(),
            &created_at,
            calendar_sync_enabled,
            calendar_sync.map(|sync| sync.calendar_id()),
        );

        if path.exists() {
            let existing = fs::read_to_string(&path)?;
            let parsed = parse_markdown(&existing);
            let existing_status = parsed
                .metadata
                .task_status
                .as_deref()
                .or(parsed.metadata.reminder_status.as_deref());
            let same_semantics = parsed.metadata.id.as_deref() == Some(note_id.as_str())
                && parsed.metadata.memory_kind.as_deref() == Some(kind.memory_kind())
                && parsed.metadata.title.as_deref() == Some(title.trim())
                && existing_status == Some(status)
                && parsed.metadata.due_at.as_deref() == due_at.as_deref()
                && body_matches_requested(&parsed.body, body);
            if same_semantics {
                if kind == ItemKind::Reminder && calendar_sync_enabled {
                    if let Some(sync) = calendar_sync {
                        // An idempotent create must not create a second Google event
                        // when the reminder already has one. Reuse the update path,
                        // which creates an event only when no event ID exists.
                        sync_updated_reminder(&path, sync, title, body, due_at.as_deref())?;
                    }
                }
                let sync_metadata = if kind == ItemKind::Reminder {
                    Some(read_calendar_sync_metadata(&path)?)
                } else {
                    None
                };
                return Ok(ToolResult {
                    success: true,
                    output: serde_json::json!({
                        "operation": "create",
                        "created": false,
                        "idempotent": true,
                        "id": public_id(kind, &note_id),
                        "note_id": note_id,
                        "path": path.strip_prefix(root)?.to_string_lossy().replace('\\', "/"),
                        "status": status,
                        "due_at": due_at,
                        "due_at_local": due_at.as_deref().map(format_local).transpose()?,
                        "timezone_offset": offset_display(local_offset()?),
                        "calendar_sync_enabled": sync_metadata.as_ref().map(|m| m.enabled),
                        "calendar_sync_status": sync_metadata.as_ref().and_then(|m| m.status.clone()),
                        "calendar_sync_error": sync_metadata.as_ref().and_then(|m| m.error.clone()),
                        "google_event_id": sync_metadata.as_ref().and_then(|m| m.event_id.clone()),
                    }),
                });
            }
            return Err("A task/reminder with the deterministic target path already exists with different task/reminder content.".into());
        }

        if !create_new_file(&path, &content)? {
            return Err("Task/reminder was created concurrently; retry the request.".into());
        }

        if kind == ItemKind::Reminder && calendar_sync_enabled {
            if let Some(sync) = calendar_sync {
                sync_created_reminder(&path, sync, title, body, due_at.as_deref())?;
            }
        }

        let sync_metadata = if kind == ItemKind::Reminder {
            Some(read_calendar_sync_metadata(&path)?)
        } else {
            None
        };

        return Ok(ToolResult {
            success: true,
            output: serde_json::json!({
                "operation": "create",
                "created": true,
                "id": public_id(kind, &note_id),
                "note_id": note_id,
                "path": path.strip_prefix(root)?.to_string_lossy().replace('\\', "/"),
                "status": status,
                "due_at": due_at,
                "due_at_local": due_at.as_deref().map(format_local).transpose()?,
                "timezone_offset": offset_display(local_offset()?),
                "calendar_sync_enabled": sync_metadata.as_ref().map(|m| m.enabled),
                "calendar_sync_status": sync_metadata.as_ref().and_then(|m| m.status.clone()),
                "calendar_sync_error": sync_metadata.as_ref().and_then(|m| m.error.clone()),
                "google_event_id": sync_metadata.as_ref().and_then(|m| m.event_id.clone()),
            }),
        });
    }

    let id = public_id(
        kind,
        arguments.id.as_deref().ok_or("Mutation requires id.")?,
    );
    let record_note = match kind {
        ItemKind::Task => db
            .task(&id)?
            .ok_or_else(|| format!("Task '{id}' was not found."))
            .map(|task| {
                (
                    task.note_id,
                    task.title,
                    task.body,
                    task.status,
                    task.due_at,
                    task.updated_at,
                    task.id,
                )
            })?,
        ItemKind::Reminder => db
            .reminder(&id)?
            .ok_or_else(|| format!("Reminder '{id}' was not found."))
            .map(|reminder| {
                (
                    reminder.note_id,
                    reminder.title,
                    reminder.body,
                    reminder.status,
                    reminder.due_at,
                    reminder.updated_at,
                    reminder.id,
                )
            })?,
    };

    let (
        note_id,
        current_title,
        _current_body,
        current_status,
        current_due_at,
        current_updated_at,
        current_db_id,
    ) = record_note;
    let relative_path = db
        .note_path(&note_id)?
        .ok_or_else(|| format!("Source note for {id} was not found."))?;
    let path = ensure_safe_existing_path(root, &relative_path)?;

    let mut next_status = current_status.clone();
    match operation.as_str() {
        "complete" => next_status = "completed".to_string(),
        "cancel" => next_status = "cancelled".to_string(),
        "trigger" => next_status = "triggered".to_string(),
        "update" => {
            if let Some(status) = arguments.status.as_deref() {
                next_status = status.to_string();
            }
        }
        _ => {}
    }

    if !transition_allowed(kind, &current_status, &next_status) {
        return Err(format!(
            "Invalid {} status transition: {} -> {}.",
            kind.memory_kind(),
            current_status,
            next_status
        )
        .into());
    }

    let new_due = match arguments.due.as_ref() {
        None => None,
        Some(None) => Some(None),
        Some(Some(expression)) => Some(Some(parse_due_expression(
            expression,
            OffsetDateTime::now_utc(),
        )?)),
    };

    let resulting_due_at = match new_due.as_ref() {
        None => current_due_at.clone(),
        Some(value) => value.clone(),
    };
    let resulting_title = arguments
        .title
        .clone()
        .unwrap_or_else(|| current_title.clone());
    let resulting_body = arguments
        .body
        .clone()
        .unwrap_or_else(|| _current_body.clone());
    let calendar_metadata = if kind == ItemKind::Reminder {
        Some(read_calendar_sync_metadata(&path)?)
    } else {
        None
    };
    let resulting_calendar_sync_enabled = arguments
        .calendar_sync
        .or_else(|| calendar_metadata.as_ref().map(|metadata| metadata.enabled))
        .unwrap_or(false);

    mutate_markdown(
        kind,
        &path,
        &note_id,
        &current_title,
        &current_status,
        current_due_at.as_deref(),
        current_updated_at.as_deref(),
        arguments.title.as_deref(),
        arguments.body.as_deref(),
        &next_status,
        new_due.as_ref().map(|value| value.as_deref()),
    )?;

    if kind == ItemKind::Reminder {
        if let Some(sync) = calendar_sync {
            let mut next_metadata = calendar_metadata.clone().unwrap_or_default();
            if let Some(requested) = arguments.calendar_sync {
                next_metadata.enabled = requested;
                next_metadata.calendar_id = next_metadata
                    .calendar_id
                    .or_else(|| Some(sync.calendar_id().to_string()));
                next_metadata.error = None;
                next_metadata.status = Some(if requested {
                    "pending".to_string()
                } else if next_metadata.event_id.is_some() {
                    "preserved".to_string()
                } else {
                    "disabled".to_string()
                });
                persist_calendar_sync_metadata(&path, &next_metadata)?;
            }

            if resulting_calendar_sync_enabled {
                match operation.as_str() {
                    "update" => {
                        sync_updated_reminder(
                            &path,
                            sync,
                            &resulting_title,
                            &resulting_body,
                            resulting_due_at.as_deref(),
                        )?;
                    }
                    "cancel" => {
                        sync_cancelled_reminder(&path)?;
                    }
                    _ => {}
                }
            }
        }
    }

    let sync_metadata = if kind == ItemKind::Reminder {
        Some(read_calendar_sync_metadata(&path)?)
    } else {
        None
    };

    Ok(ToolResult {
        success: true,
        output: serde_json::json!({
            "operation": operation,
            "updated": true,
            "id": current_db_id,
            "note_id": note_id,
            "path": relative_path,
            "title": resulting_title,
            "body": resulting_body,
            "status": next_status,
            "due_at": resulting_due_at,
            "due_at_local": resulting_due_at.as_deref().map(format_local).transpose()?,
            "timezone_offset": offset_display(local_offset()?),
            "calendar_sync_enabled": sync_metadata.as_ref().map(|m| m.enabled),
            "calendar_sync_status": sync_metadata.as_ref().and_then(|m| m.status.clone()),
            "calendar_sync_error": sync_metadata.as_ref().and_then(|m| m.error.clone()),
            "google_event_id": sync_metadata.as_ref().and_then(|m| m.event_id.clone()),
        }),
    })
}

pub struct TaskListTool {
    db: SqliteMemoryDb,
}

impl TaskListTool {
    pub fn new(db_path: impl AsRef<Path>) -> Result<Self> {
        let db = SqliteMemoryDb::open(db_path)?;
        db.initialize_schema()?;
        Ok(Self { db })
    }
}

impl Tool for TaskListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tasks.list".to_string(),
            description: "List or inspect Markdown-backed tasks. Supports status, text, due-date, and exact-id filtering.".to_string(),
            permission: ToolPermission::ReadOnly,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let parsed: ListArguments = serde_json::from_value(arguments.clone())?;
        list_items(&self.db, ItemKind::Task, parsed)
    }
}

pub struct TaskMutationTool {
    root: PathBuf,
    db: SqliteMemoryDb,
}

impl TaskMutationTool {
    pub fn new(root: impl AsRef<Path>, db_path: impl AsRef<Path>) -> Result<Self> {
        let db = SqliteMemoryDb::open(db_path)?;
        db.initialize_schema()?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            db,
        })
    }
}

impl Tool for TaskMutationTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tasks.mutate".to_string(),
            description: "Create, update, complete, or cancel Markdown-backed tasks. Mutations require approval.".to_string(),
            permission: ToolPermission::ApprovalRequired,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let parsed: MutationArguments = serde_json::from_value(arguments.clone())?;
        mutate_item(&self.root, &self.db, ItemKind::Task, parsed, None)
    }
}

pub struct ReminderListTool {
    db: SqliteMemoryDb,
}

impl ReminderListTool {
    pub fn new(db_path: impl AsRef<Path>) -> Result<Self> {
        let db = SqliteMemoryDb::open(db_path)?;
        db.initialize_schema()?;
        Ok(Self { db })
    }
}

impl Tool for ReminderListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "reminders.list".to_string(),
            description: "List or inspect Markdown-backed reminders. Supports status, text, due-date, and exact-id filtering.".to_string(),
            permission: ToolPermission::ReadOnly,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let parsed: ListArguments = serde_json::from_value(arguments.clone())?;
        list_items(&self.db, ItemKind::Reminder, parsed)
    }
}

pub struct ReminderMutationTool {
    root: PathBuf,
    db: SqliteMemoryDb,
    calendar_sync: ReminderCalendarSync,
}

impl ReminderMutationTool {
    pub fn new(root: impl AsRef<Path>, db_path: impl AsRef<Path>) -> Result<Self> {
        let db = SqliteMemoryDb::open(db_path)?;
        db.initialize_schema()?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            db,
            calendar_sync: ReminderCalendarSync::from_env()?,
        })
    }

    #[cfg(test)]
    fn with_calendar_sync(
        root: impl AsRef<Path>,
        db_path: impl AsRef<Path>,
        calendar_sync: ReminderCalendarSync,
    ) -> Result<Self> {
        let db = SqliteMemoryDb::open(db_path)?;
        db.initialize_schema()?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            db,
            calendar_sync,
        })
    }
}

impl Tool for ReminderMutationTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "reminders.mutate".to_string(),
            description: "Create, update, complete, cancel, or trigger Markdown-backed reminders. Mutations require approval.".to_string(),
            permission: ToolPermission::ApprovalRequired,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let parsed: MutationArguments = serde_json::from_value(arguments.clone())?;
        mutate_item(
            &self.root,
            &self.db,
            ItemKind::Reminder,
            parsed,
            Some(&self.calendar_sync),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn start_calendar_create_server() -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("calendar request expected");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];

            loop {
                let read = stream.read(&mut buffer).expect("request read failed");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length:")
                                .or_else(|| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let header_length = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .expect("header terminator missing")
                        + 4;
                    if request.len() >= header_length + content_length {
                        break;
                    }
                }
            }

            let body = r#"{"id":"event-123","summary":"Call Mom","start":{"dateTime":"2026-09-23T10:00:00Z"},"end":{"dateTime":"2026-09-23T10:30:00Z"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("response write failed");
        });
        Ok((format!("http://{address}/calendar/v3"), handle))
    }

    fn database() -> Result<(tempfile::TempDir, PathBuf, SqliteMemoryDb)> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("memory");
        fs::create_dir_all(&root)?;
        let db_path = temp.path().join("assistant.db");
        let db = SqliteMemoryDb::open(&db_path)?;
        db.initialize_schema()?;
        Ok((temp, root, db))
    }

    fn refresh(root: &Path, db: &mut SqliteMemoryDb) -> Result<()> {
        let snapshot = indexer::build_snapshot_from(root)?;
        db.sync_snapshot(&snapshot)?;
        db.sync_semantics(&snapshot)?;
        Ok(())
    }

    #[test]
    fn parses_local_tomorrow_and_converts_to_utc() -> Result<()> {
        let now = OffsetDateTime::parse("2026-09-13T10:00:00Z", &Rfc3339)?;
        let due = parse_due_expression("tomorrow at 6 PM", now)?;
        let parsed = OffsetDateTime::parse(&due, &Rfc3339)?;
        let local = parsed.to_offset(local_offset()?);
        assert_eq!(
            local.date(),
            now.to_offset(local_offset()?).date().next_day().unwrap()
        );
        assert_eq!(local.time().hour(), 18);
        assert_eq!(local.time().minute(), 0);
        Ok(())
    }

    #[test]
    fn parses_today_with_standalone_time() -> Result<()> {
        let now = OffsetDateTime::parse("2026-09-16T07:00:00Z", &Rfc3339)?;
        let due = parse_due_expression("at 12:45 PM", now)?;
        let local = OffsetDateTime::parse(&due, &Rfc3339)?.to_offset(local_offset()?);

        assert_eq!(local.date(), now.to_offset(local_offset()?).date());
        assert_eq!(local.time().hour(), 12);
        assert_eq!(local.time().minute(), 45);
        Ok(())
    }

    #[test]
    fn parses_weekday_with_time() -> Result<()> {
        let now = OffsetDateTime::parse("2026-09-13T10:00:00Z", &Rfc3339)?;
        let due = parse_due_expression("Friday 5 PM", now)?;
        let local = OffsetDateTime::parse(&due, &Rfc3339)?.to_offset(local_offset()?);
        assert_eq!(local.date().weekday(), Weekday::Friday);
        assert_eq!(local.time().hour(), 17);
        Ok(())
    }

    #[test]
    fn parses_dd_mm_yyyy_with_time() -> Result<()> {
        let now = OffsetDateTime::parse("2026-09-13T10:00:00Z", &Rfc3339)?;
        let due = parse_due_expression("24/09/2026 16:30", now)?;
        let local = OffsetDateTime::parse(&due, &Rfc3339)?.to_offset(local_offset()?);
        assert_eq!(local.date().year(), 2026);
        assert_eq!(local.date().month(), Month::September);
        assert_eq!(local.date().day(), 24);
        assert_eq!(local.time().hour(), 16);
        assert_eq!(local.time().minute(), 30);
        Ok(())
    }

    #[test]
    fn parses_dd_mm_yy_hyphen_with_time() -> Result<()> {
        let now = OffsetDateTime::parse("2026-09-23T10:00:00Z", &Rfc3339)?;
        let due = parse_due_expression("24-09-26 19:00", now)?;
        let local = OffsetDateTime::parse(&due, &Rfc3339)?.to_offset(local_offset()?);
        assert_eq!(local.date().year(), 2026);
        assert_eq!(local.date().month(), Month::September);
        assert_eq!(local.date().day(), 24);
        assert_eq!(local.time().hour(), 19);
        assert_eq!(local.time().minute(), 0);
        Ok(())
    }

    #[test]
    fn dd_mm_yyyy_defaults_to_nine_am() -> Result<()> {
        let now = OffsetDateTime::parse("2026-09-13T10:00:00Z", &Rfc3339)?;
        let due = parse_due_expression("24/09/2026", now)?;
        let local = OffsetDateTime::parse(&due, &Rfc3339)?.to_offset(local_offset()?);
        assert_eq!(local.date().day(), 24);
        assert_eq!(local.time().hour(), 9);
        assert_eq!(local.time().minute(), 0);
        Ok(())
    }

    #[test]
    fn parses_relative_duration() -> Result<()> {
        let now = OffsetDateTime::parse("2026-09-13T10:00:00Z", &Rfc3339)?;
        let due = parse_due_expression("in 2 hours", now)?;
        assert_eq!(due, "2026-09-13T12:00:00Z");
        Ok(())
    }

    #[test]
    fn parses_relative_duration_with_articles() -> Result<()> {
        let now = OffsetDateTime::parse("2026-09-23T10:00:00Z", &Rfc3339)?;

        assert_eq!(
            parse_due_expression("in an hour", now)?,
            "2026-09-23T11:00:00Z"
        );
        assert_eq!(
            parse_due_expression("in a day", now)?,
            "2026-09-24T10:00:00Z"
        );
        assert_eq!(
            parse_due_expression("in half an hour", now)?,
            "2026-09-23T10:30:00Z"
        );
        Ok(())
    }

    #[test]
    fn rejects_malformed_due_expression() {
        let now = OffsetDateTime::now_utc();
        assert!(parse_due_expression("not a date", now).is_err());
        assert!(parse_due_expression("in bananas hours", now).is_err());
        assert!(parse_due_expression("tomorrow at 99 PM", now).is_err());
    }

    #[test]
    fn task_create_is_idempotent() -> Result<()> {
        let (_temp, root, db) = database()?;
        let tool = TaskMutationTool {
            root: root.clone(),
            db,
        };
        let arguments = serde_json::json!({
            "operation": "create",
            "title": "Review RCM report",
            "body": "Review the report.",
            "due": "tomorrow at 6 PM"
        });
        let first = tool.execute(&arguments)?;
        let second = tool.execute(&arguments)?;
        assert_eq!(first.output["created"], true);
        assert_eq!(second.output["idempotent"], true);
        Ok(())
    }

    #[test]
    fn task_mutation_preserves_unknown_frontmatter() -> Result<()> {
        let (_temp, root, mut db) = database()?;
        let note = root.join("journal/tasks/task-custom.md");
        fs::create_dir_all(note.parent().unwrap())?;
        fs::write(
            &note,
            r#"---
id: "task-custom"
title: "Original"
type: "journal"
custom_field: "preserve me"
memory_kind: "task"
task_status: "open"
due_at: "2026-09-15T10:00:00Z"
created_at: "2026-09-13T10:00:00Z"
updated_at: "2026-09-13T10:00:00Z"
---

Keep this body.
"#,
        )?;
        refresh(&root, &mut db)?;
        let tool = TaskMutationTool {
            root: root.clone(),
            db,
        };
        let result = tool.execute(&serde_json::json!({
            "operation": "update",
            "id": "task:task-custom",
            "title": "Updated",
            "due": "2026-09-16T10:00:00Z"
        }))?;
        assert!(result.success);
        let contents = fs::read_to_string(note)?;
        assert!(contents.contains("custom_field: \"preserve me\""));
        assert!(contents.contains("title: \"Updated\""));
        assert!(contents.contains("task_status: \"open\""));
        assert!(contents.contains("due_at: \"2026-09-16T10:00:00Z\""));
        assert!(contents.contains("Keep this body."));
        Ok(())
    }

    #[test]
    fn task_mutation_follows_authoritative_path_outside_task_directory() -> Result<()> {
        let (_temp, root, mut db) = database()?;
        let note = root.join("journal/projects/rcm.md");
        fs::create_dir_all(note.parent().unwrap())?;
        fs::write(
            &note,
            r#"---
id: "task-in-project"
title: "RCM task"
type: "journal"
namespace: "global"
status: "active"
memory_kind: "task"
task_status: "open"
created_at: "2026-09-13T10:00:00Z"
updated_at: "2026-09-13T10:00:00Z"
---

Review RCM data.
"#,
        )?;
        refresh(&root, &mut db)?;
        let tool = TaskMutationTool { root, db };
        let result = tool.execute(&serde_json::json!({
            "operation": "complete",
            "id": "task:task-in-project"
        }))?;
        assert!(result.success);
        assert!(fs::read_to_string(note)?.contains("task_status: \"completed\""));
        Ok(())
    }

    #[test]
    fn task_completion_updates_markdown_then_reindexes() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let tool = TaskMutationTool {
            root: root.clone(),
            db: SqliteMemoryDb::open(temp.path().join("assistant.db"))?,
        };
        let created = tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Finish indexing",
            "body": "Complete the indexing work."
        }))?;
        let id = created.output["id"].as_str().unwrap().to_string();
        refresh(&root, &mut db)?;
        let completed = TaskMutationTool {
            root: root.clone(),
            db: SqliteMemoryDb::open(temp.path().join("assistant.db"))?,
        };
        completed.execute(&serde_json::json!({
            "operation": "complete",
            "id": id
        }))?;
        refresh(&root, &mut db)?;
        assert_eq!(db.tasks(Some("completed"), 10)?.len(), 1);
        assert_eq!(db.tasks(Some("open"), 10)?.len(), 0);
        Ok(())
    }

    #[test]
    fn task_can_be_cancelled_and_reindexed() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let tool = TaskMutationTool::new(&root, temp.path().join("assistant.db"))?;
        let created = tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Cancel this task"
        }))?;
        refresh(&root, &mut db)?;
        tool.execute(&serde_json::json!({
            "operation": "cancel",
            "id": created.output["id"].as_str().unwrap()
        }))?;
        refresh(&root, &mut db)?;
        assert_eq!(db.tasks(Some("cancelled"), 10)?.len(), 1);
        assert_eq!(db.tasks(Some("open"), 10)?.len(), 0);
        Ok(())
    }

    #[test]
    fn task_listing_filters_status_query_and_due_date() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let tool = TaskMutationTool::new(&root, temp.path().join("assistant.db"))?;
        tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Due tomorrow",
            "due": "tomorrow at 10 AM"
        }))?;
        tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "No deadline",
            "body": "A different task"
        }))?;
        refresh(&root, &mut db)?;

        let list = TaskListTool::new(temp.path().join("assistant.db"))?;
        let result = list.execute(&serde_json::json!({
            "status": "open",
            "query": "Due tomorrow",
            "due": "tomorrow",
            "limit": 10
        }))?;
        assert_eq!(result.output["count"], 1);
        assert_eq!(result.output["items"][0]["title"], "Due tomorrow");
        Ok(())
    }

    #[test]
    fn reminder_identity_query_matches_inflected_wording() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let tool = ReminderMutationTool::new(&root, temp.path().join("assistant.db"))?;
        tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Submit the application",
        }))?;
        refresh(&root, &mut db)?;

        let list = ReminderListTool::new(temp.path().join("assistant.db"))?;
        let result = list.execute(&serde_json::json!({
            "query": "submitting the application",
            "limit": 10,
        }))?;

        assert_eq!(result.output["count"], 1);
        assert_eq!(result.output["items"][0]["title"], "Submit the application");
        Ok(())
    }

    #[test]
    fn reminder_create_syncs_calendar_event_and_persists_metadata() -> Result<()> {
        let (temp, root, _db) = database()?;
        let (base_url, server) = start_calendar_create_server()?;
        let client =
            crate::GoogleCalendarClient::with_base_url("test-token", "primary", &base_url)?;
        let tool = ReminderMutationTool::with_calendar_sync(
            &root,
            temp.path().join("assistant.db"),
            ReminderCalendarSync::for_test(client, "primary"),
        )?;
        let created = tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Call Mom",
            "due": "2026-09-23T10:00:00Z",
            "calendar_sync": true
        }))?;

        let relative_path = created.output["path"].as_str().unwrap();
        let note = fs::read_to_string(root.join(relative_path))?;
        let parsed = parse_markdown(&note);
        assert_eq!(parsed.metadata.calendar_sync_enabled, Some(true));
        assert_eq!(
            parsed.metadata.google_calendar_id.as_deref(),
            Some("primary")
        );
        assert_eq!(
            parsed.metadata.google_event_id.as_deref(),
            Some("event-123")
        );
        assert_eq!(
            parsed.metadata.calendar_sync_status.as_deref(),
            Some("synced")
        );
        assert!(parsed.metadata.calendar_last_synced_at.is_some());
        assert_eq!(parsed.metadata.calendar_sync_error.as_deref(), None);
        assert_eq!(created.output["calendar_sync_enabled"], true);
        assert_eq!(created.output["calendar_sync_status"], "synced");
        assert_eq!(created.output["google_event_id"], "event-123");

        server.join().expect("calendar mock server panicked");
        Ok(())
    }

    fn start_calendar_create_then_update_server() -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || {
            for (index, (expected_method, expected_path, response_body)) in [
                (
                    "POST",
                    "/calendar/v3/calendars/primary/events?sendUpdates=none",
                    r#"{"id":"event-123","summary":"Call Mom"}"#,
                ),
                (
                    "PATCH",
                    "/calendar/v3/calendars/primary/events/event-123?sendUpdates=none",
                    r#"{"id":"event-123","summary":"Call Mom"}"#,
                ),
            ]
            .iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().expect("calendar request expected");
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).expect("request read failed");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("Content-Length:")
                                    .or_else(|| line.strip_prefix("content-length:"))
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        let header_length = request
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .expect("header terminator missing")
                            + 4;
                        if request.len() >= header_length + content_length {
                            break;
                        }
                    }
                }

                let first_line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                assert!(
                    first_line.starts_with(&format!("{expected_method} {expected_path} ")),
                    "request {} had unexpected line: {first_line}",
                    index + 1
                );

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("response write failed");
            }
        });
        Ok((format!("http://{address}/calendar/v3"), handle))
    }

    #[test]
    fn reminder_idempotent_create_updates_existing_calendar_event() -> Result<()> {
        let (temp, root, _db) = database()?;
        let (base_url, server) = start_calendar_create_then_update_server()?;
        let client =
            crate::GoogleCalendarClient::with_base_url("test-token", "primary", &base_url)?;
        let tool = ReminderMutationTool::with_calendar_sync(
            &root,
            temp.path().join("assistant.db"),
            ReminderCalendarSync::for_test(client, "primary"),
        )?;

        let arguments = serde_json::json!({
            "operation": "create",
            "title": "Call Mom",
            "due": "2026-09-23T10:00:00Z",
            "calendar_sync": true
        });
        let first = tool.execute(&arguments)?;
        assert_eq!(first.output["google_event_id"], "event-123");

        let second = tool.execute(&arguments)?;
        assert_eq!(second.output["google_event_id"], "event-123");
        assert_eq!(second.output["calendar_sync_status"], "synced");

        server.join().expect("calendar mock server panicked");
        Ok(())
    }

    #[test]
    fn reminder_create_without_calendar_sync_stays_local_only() -> Result<()> {
        let (temp, root, _db) = database()?;
        let client = crate::GoogleCalendarClient::with_base_url(
            "test-token",
            "primary",
            "http://127.0.0.1:9/calendar/v3",
        )?;
        let tool = ReminderMutationTool::with_calendar_sync(
            &root,
            temp.path().join("assistant.db"),
            ReminderCalendarSync::for_test(client, "primary"),
        )?;

        let created = tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Local only",
            "due": "2026-09-23T10:00:00Z"
        }))?;

        let relative_path = created.output["path"].as_str().unwrap();
        let parsed = parse_markdown(&fs::read_to_string(root.join(relative_path))?);
        assert_eq!(parsed.metadata.calendar_sync_enabled, Some(false));
        assert_eq!(
            parsed.metadata.calendar_sync_status.as_deref(),
            Some("disabled")
        );
        assert_eq!(parsed.metadata.google_event_id, None);
        assert_eq!(created.output["calendar_sync_enabled"], false);
        assert_eq!(created.output["calendar_sync_status"], "disabled");
        Ok(())
    }

    #[test]
    fn reminder_calendar_event_is_preserved_on_local_cancel_and_due_clear() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let create_tool = ReminderMutationTool::new(&root, temp.path().join("assistant.db"))?;
        let created = create_tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Preserve calendar event",
            "due": "2026-09-23T10:00:00Z"
        }))?;

        let relative_path = created.output["path"].as_str().unwrap();
        let path = root.join(relative_path);
        let mut content = fs::read_to_string(&path)?;
        content = set_top_level_field(&content, "calendar_sync_enabled", Some("true"))?;
        content = set_top_level_field(&content, "google_calendar_id", Some("primary"))?;
        content = set_top_level_field(&content, "google_event_id", Some("event-preserve"))?;
        content = set_top_level_field(&content, "calendar_sync_status", Some("synced"))?;
        fs::write(&path, content)?;
        refresh(&root, &mut db)?;
        // The mutation tool opens a fresh SQLite connection. Drop the refresh
        // connection before opening it so this test cannot retain a stale
        // SQLite snapshot while validating Markdown/index consistency.
        drop(db);

        let client = crate::GoogleCalendarClient::with_base_url(
            "test-token",
            "primary",
            "http://127.0.0.1:9/calendar/v3",
        )?;
        let tool = ReminderMutationTool::with_calendar_sync(
            &root,
            temp.path().join("assistant.db"),
            ReminderCalendarSync::for_test(client, "primary"),
        )?;

        tool.execute(&serde_json::json!({
            "operation": "update",
            "id": created.output["id"].as_str().unwrap(),
            "due": null
        }))?;

        // Mutations are deliberately followed by an explicit index refresh in
        // this test so a second mutation observes the authoritative Markdown
        // state rather than the mutation tool's pre-update SQLite snapshot.
        let mut refreshed_db = SqliteMemoryDb::open(temp.path().join("assistant.db"))?;
        refresh(&root, &mut refreshed_db)?;
        drop(refreshed_db);

        let content = fs::read_to_string(&path)?;
        let parsed = parse_markdown(&content);
        assert_eq!(
            parsed.metadata.google_event_id.as_deref(),
            Some("event-preserve")
        );
        assert_eq!(
            parsed.metadata.calendar_sync_status.as_deref(),
            Some("preserved")
        );

        tool.execute(&serde_json::json!({
            "operation": "cancel",
            "id": created.output["id"].as_str().unwrap()
        }))?;

        let content = fs::read_to_string(&path)?;
        let parsed = parse_markdown(&content);
        assert_eq!(
            parsed.metadata.google_event_id.as_deref(),
            Some("event-preserve")
        );
        assert_eq!(
            parsed.metadata.calendar_sync_status.as_deref(),
            Some("preserved")
        );
        Ok(())
    }

    #[test]
    fn reminder_create_and_cancel() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let tool = ReminderMutationTool::new(&root, temp.path().join("assistant.db"))?;
        let created = tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Submit application",
            "due": "tomorrow at 5 PM"
        }))?;
        refresh(&root, &mut db)?;
        tool.execute(&serde_json::json!({
            "operation": "cancel",
            "id": created.output["id"].as_str().unwrap()
        }))?;
        refresh(&root, &mut db)?;
        assert_eq!(db.reminders(Some("cancelled"), 10)?.len(), 1);
        Ok(())
    }

    #[test]
    fn reminder_can_be_triggered_and_completed() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let tool = ReminderMutationTool::new(&root, temp.path().join("assistant.db"))?;
        let created = tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Review reminder"
        }))?;
        refresh(&root, &mut db)?;
        let id = created.output["id"].as_str().unwrap();
        tool.execute(&serde_json::json!({"operation": "trigger", "id": id}))?;
        refresh(&root, &mut db)?;
        assert_eq!(db.reminders(Some("triggered"), 10)?.len(), 1);
        tool.execute(&serde_json::json!({"operation": "complete", "id": id}))?;
        refresh(&root, &mut db)?;
        assert_eq!(db.reminders(Some("completed"), 10)?.len(), 1);
        Ok(())
    }

    #[test]
    fn reminder_due_can_be_cleared_and_updated() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let tool = ReminderMutationTool::new(&root, temp.path().join("assistant.db"))?;
        let created = tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Follow up",
            "due": "tomorrow at 5 PM"
        }))?;
        refresh(&root, &mut db)?;
        let id = created.output["id"].as_str().unwrap();

        tool.execute(&serde_json::json!({
            "operation": "update",
            "id": id,
            "due": null
        }))?;
        refresh(&root, &mut db)?;
        assert_eq!(db.reminders(Some("scheduled"), 10)?[0].due_at, None);
        Ok(())
    }

    #[test]
    fn invalid_transition_is_rejected() -> Result<()> {
        let (temp, root, mut db) = database()?;
        let tool = TaskMutationTool::new(&root, temp.path().join("assistant.db"))?;
        let created = tool.execute(&serde_json::json!({
            "operation": "create",
            "title": "Complete once"
        }))?;
        refresh(&root, &mut db)?;
        tool.execute(&serde_json::json!({
            "operation": "complete",
            "id": created.output["id"].as_str().unwrap()
        }))?;
        refresh(&root, &mut db)?;
        assert!(
            tool.execute(&serde_json::json!({
                "operation": "complete",
                "id": created.output["id"].as_str().unwrap()
            }))
            .is_ok()
        );
        assert!(
            tool.execute(&serde_json::json!({
                "operation": "cancel",
                "id": created.output["id"].as_str().unwrap()
            }))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn nonexistent_item_id_is_rejected() -> Result<()> {
        let (_temp, root, db) = database()?;
        let tool = TaskMutationTool { root, db };
        let error = tool
            .execute(&serde_json::json!({
                "operation": "complete",
                "id": "task:does-not-exist"
            }))
            .expect_err("missing task should be rejected")
            .to_string();
        assert!(error.contains("was not found"));
        Ok(())
    }

    #[test]
    fn explicit_null_due_clears_existing_due_date() -> Result<()> {
        let parsed: MutationArguments = serde_json::from_value(serde_json::json!({
            "operation": "update",
            "id": "task:123",
            "due": null
        }))?;
        assert_eq!(parsed.due, Some(None));
        Ok(())
    }

    #[test]
    fn malformed_due_date_is_rejected_during_mutation() -> Result<()> {
        let (_temp, root, db) = database()?;
        let tool = TaskMutationTool { root, db };
        let error = tool
            .execute(&serde_json::json!({
                "operation": "create",
                "title": "Bad due",
                "due": "not a real date"
            }))
            .expect_err("malformed due expression should be rejected")
            .to_string();
        assert!(error.contains("Unsupported weekday expression") || error.contains("YYYY-MM-DD"));
        Ok(())
    }

    #[test]
    fn due_update_preserves_body_exactly() -> Result<()> {
        let (_temp, root, mut db) = database()?;
        let note = root.join("journal/tasks/task-body.md");
        fs::create_dir_all(note.parent().unwrap())?;
        let original = r#"---
        id: "task-body"
        title: "Body test"
        type: "journal"
        memory_kind: "task"
        task_status: "open"
        due_at: "2026-09-15T10:00:00Z"
        created_at: "2026-09-13T10:00:00Z"
        updated_at: "2026-09-13T10:00:00Z"
        ---

        line one

        line two
        "#;
        fs::write(&note, original.replace("        ", ""))?;
        refresh(&root, &mut db)?;
        let tool = TaskMutationTool {
            root: root.clone(),
            db,
        };
        tool.execute(&serde_json::json!({
            "operation": "update",
            "id": "task:task-body",
            "due": "2026-09-16T10:00:00Z"
        }))?;
        let updated = fs::read_to_string(note)?;
        assert!(updated.ends_with("\nline one\n\nline two\n"));
        Ok(())
    }

    #[test]
    fn path_traversal_is_rejected_from_indexed_note_path() -> Result<()> {
        let (_temp, root, _db) = database()?;
        let error = ensure_safe_existing_path(&root, "journal/tasks/../evil.md")
            .expect_err("path traversal should be rejected")
            .to_string();
        assert!(error.contains("relative Markdown") || error.contains("escapes"));
        Ok(())
    }
}
