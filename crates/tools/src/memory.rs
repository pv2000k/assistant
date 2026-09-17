use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteEntity {
    pub name: String,
    #[serde(rename = "type")]
    pub entity_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteRelationship {
    pub source: String,
    pub target: String,
    pub relationship: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub occurred_at: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteState {
    pub entity: String,
    pub state: String,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteRequest {
    pub path: String,
    pub title: String,
    #[serde(rename = "type")]
    pub note_type: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub entities: Vec<MemoryWriteEntity>,
    #[serde(default)]
    pub relationships: Vec<MemoryWriteRelationship>,
    #[serde(default)]
    pub events: Vec<MemoryWriteEvent>,
    #[serde(default)]
    pub states: Vec<MemoryWriteState>,
    #[serde(default)]
    pub memory_kind: Option<String>,
    #[serde(default)]
    pub task_status: Option<String>,
    #[serde(default)]
    pub reminder_status: Option<String>,
    #[serde(default)]
    pub due_at: Option<String>,
    #[serde(default)]
    pub source_type: Option<String>,
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub source_anchor: Option<String>,
    #[serde(default)]
    pub source_excerpt: Option<String>,
}

impl MemoryWriteRequest {
    pub fn validate_structured_metadata(&self) -> Result<()> {
        if self.entities.len() > 8 {
            return Err("memory.write accepts at most 8 entities.".into());
        }
        if self.relationships.len() > 8 {
            return Err("memory.write accepts at most 8 relationships.".into());
        }
        if self.events.len() > 6 {
            return Err("memory.write accepts at most 6 events.".into());
        }
        if self.states.len() > 8 {
            return Err("memory.write accepts at most 8 states.".into());
        }

        for entity in &self.entities {
            if entity.name.trim().is_empty()
                || entity.name.chars().count() > 128
                || entity.name.contains(['\n', '\r'])
                || entity.entity_type.trim().is_empty()
                || entity.entity_type.chars().count() > 64
                || entity.entity_type.contains(['\n', '\r'])
            {
                return Err("memory.write entities must have non-empty, single-line name/type values within limits.".into());
            }
        }

        for relationship in &self.relationships {
            if relationship.source.trim().is_empty()
                || relationship.source.chars().count() > 128
                || relationship.source.contains(['\n', '\r'])
                || relationship.target.trim().is_empty()
                || relationship.target.chars().count() > 128
                || relationship.target.contains(['\n', '\r'])
                || relationship.relationship.trim().is_empty()
                || relationship.relationship.chars().count() > 64
                || relationship.relationship.contains(['\n', '\r'])
            {
                return Err("memory.write relationships must have non-empty, single-line values within limits.".into());
            }
        }

        for event in &self.events {
            if event.event_type.trim().is_empty()
                || event.event_type.chars().count() > 64
                || event.event_type.contains(['\n', '\r'])
                || event.entities.len() > 8
            {
                return Err(
                    "memory.write events must have a valid type and at most 8 entity names.".into(),
                );
            }
            if let Some(value) = &event.occurred_at {
                if value.trim().is_empty()
                    || value.chars().count() > 64
                    || value.contains(['\n', '\r'])
                {
                    return Err("memory.write event occurred_at must be non-empty, single-line, and at most 64 characters.".into());
                }
            }
            if let Some(value) = &event.description {
                if value.trim().is_empty()
                    || value.chars().count() > 512
                    || value.contains(['\n', '\r'])
                {
                    return Err("memory.write event description must be non-empty, single-line, and at most 512 characters.".into());
                }
            }
            for entity in &event.entities {
                if entity.trim().is_empty()
                    || entity.chars().count() > 128
                    || entity.contains(['\n', '\r'])
                {
                    return Err("memory.write event entity names must be non-empty, single-line, and at most 128 characters.".into());
                }
            }
        }

        for (name, value, max_len) in [
            ("memory_kind", self.memory_kind.as_deref(), 64usize),
            ("task_status", self.task_status.as_deref(), 32usize),
            ("reminder_status", self.reminder_status.as_deref(), 32usize),
            ("due_at", self.due_at.as_deref(), 64usize),
            ("source_type", self.source_type.as_deref(), 64usize),
            ("source_id", self.source_id.as_deref(), 128usize),
            ("source_path", self.source_path.as_deref(), 512usize),
            ("source_anchor", self.source_anchor.as_deref(), 256usize),
            ("source_excerpt", self.source_excerpt.as_deref(), 1000usize),
        ] {
            if let Some(value) = value {
                if value.trim().is_empty()
                    || value.chars().count() > max_len
                    || value.contains(['\n', '\r'])
                {
                    return Err(format!(
                        "memory.write {name} must be non-empty, single-line, and at most {max_len} characters."
                    ).into());
                }
            }
        }

        if let Some(status) = self.task_status.as_deref()
            && !matches!(
                status,
                "open" | "in_progress" | "completed" | "cancelled" | "expired" | "archived"
            )
        {
            return Err("memory.write task_status must be open, in_progress, completed, cancelled, expired, or archived.".into());
        }

        if let Some(status) = self.reminder_status.as_deref()
            && !matches!(
                status,
                "scheduled" | "triggered" | "completed" | "cancelled"
            )
        {
            return Err("memory.write reminder_status must be scheduled, triggered, completed, or cancelled.".into());
        }

        match self.memory_kind.as_deref() {
            Some("task") => {
                if self.task_status.is_none() {
                    return Err("memory.write task memory requires task_status.".into());
                }
                if self.reminder_status.is_some() {
                    return Err("memory.write task memory cannot include reminder_status.".into());
                }
            }
            Some("reminder") => {
                if self.reminder_status.is_none() {
                    return Err("memory.write reminder memory requires reminder_status.".into());
                }
                if self.task_status.is_some() {
                    return Err("memory.write reminder memory cannot include task_status.".into());
                }
            }
            _ => {
                if self.task_status.is_some() || self.reminder_status.is_some() {
                    return Err("memory.write task_status/reminder_status require the corresponding memory_kind.".into());
                }
            }
        }

        for state in &self.states {
            if state.entity.trim().is_empty()
                || state.entity.chars().count() > 128
                || state.entity.contains(['\n', '\r'])
                || state.state.trim().is_empty()
                || state.state.chars().count() > 512
                || state.state.contains(['\n', '\r'])
            {
                return Err("memory.write states must have valid single-line entity/state values within limits.".into());
            }
            for value in [state.valid_from.as_deref(), state.valid_to.as_deref()]
                .into_iter()
                .flatten()
            {
                if value.trim().is_empty()
                    || value.chars().count() > 64
                    || value.contains(['\n', '\r'])
                {
                    return Err("memory.write state validity fields must be non-empty, single-line, and at most 64 characters.".into());
                }
            }
        }

        Ok(())
    }
}

pub struct MemoryWriteTool {
    root: PathBuf,
}

impl MemoryWriteTool {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn resolve_path(&self, requested: &str) -> Result<PathBuf> {
        let relative = Path::new(requested);

        if relative.is_absolute() {
            return Err("Memory path must be relative.".into());
        }

        if relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err("Memory path cannot contain '..'.".into());
        }

        if relative.extension() != Some(std::ffi::OsStr::new("md")) {
            return Err("Memory files must use the .md extension.".into());
        }

        fs::create_dir_all(&self.root)?;
        let canonical_root = fs::canonicalize(&self.root)?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent_path = self.root.join(parent);
        fs::create_dir_all(&parent_path)?;
        let canonical_parent = fs::canonicalize(&parent_path)?;

        if !canonical_parent.starts_with(&canonical_root) {
            return Err("Memory path escapes the configured root.".into());
        }

        Ok(canonical_parent.join(relative.file_name().ok_or("Invalid memory filename.")?))
    }

    fn resolve_new_file(&self, requested: &str) -> Result<PathBuf> {
        let path = self.resolve_path(requested)?;
        if path.exists() {
            return Err(format!("Memory file already exists: {}", requested).into());
        }
        Ok(path)
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

    fn render(request: &MemoryWriteRequest) -> String {
        let mut output = String::new();

        output.push_str("---\n");

        output.push_str("title: ");
        output.push_str(&Self::yaml_scalar(&request.title));
        output.push('\n');

        output.push_str("type: ");
        output.push_str(&Self::yaml_scalar(&request.note_type));
        output.push('\n');

        output.push_str("tags:\n");

        for tag in &request.tags {
            output.push_str("  - ");
            output.push_str(&Self::yaml_scalar(tag));
            output.push('\n');
        }

        if !request.entities.is_empty() {
            output.push_str("entities:\n");
            for entity in &request.entities {
                output.push_str("  - name: ");
                output.push_str(&Self::yaml_scalar(&entity.name));
                output.push('\n');
                output.push_str("    type: ");
                output.push_str(&Self::yaml_scalar(&entity.entity_type));
                output.push('\n');
            }
        }

        if !request.relationships.is_empty() {
            output.push_str("relationships:\n");
            for relationship in &request.relationships {
                output.push_str("  - source: ");
                output.push_str(&Self::yaml_scalar(&relationship.source));
                output.push('\n');
                output.push_str("    target: ");
                output.push_str(&Self::yaml_scalar(&relationship.target));
                output.push('\n');
                output.push_str("    relationship: ");
                output.push_str(&Self::yaml_scalar(&relationship.relationship));
                output.push('\n');
            }
        }

        if !request.events.is_empty() {
            output.push_str("events:\n");
            for event in &request.events {
                output.push_str("  - type: ");
                output.push_str(&Self::yaml_scalar(&event.event_type));
                output.push('\n');
                if let Some(occurred_at) = &event.occurred_at {
                    output.push_str("    occurred_at: ");
                    output.push_str(&Self::yaml_scalar(occurred_at));
                    output.push('\n');
                }
                if let Some(description) = &event.description {
                    output.push_str("    description: ");
                    output.push_str(&Self::yaml_scalar(description));
                    output.push('\n');
                }
                if !event.entities.is_empty() {
                    output.push_str("    entities:\n");
                    for entity in &event.entities {
                        output.push_str("      - ");
                        output.push_str(&Self::yaml_scalar(entity));
                        output.push('\n');
                    }
                }
            }
        }

        if !request.states.is_empty() {
            output.push_str("states:\n");
            for state in &request.states {
                output.push_str("  - entity: ");
                output.push_str(&Self::yaml_scalar(&state.entity));
                output.push('\n');
                output.push_str("    state: ");
                output.push_str(&Self::yaml_scalar(&state.state));
                output.push('\n');
                if let Some(valid_from) = &state.valid_from {
                    output.push_str("    valid_from: ");
                    output.push_str(&Self::yaml_scalar(valid_from));
                    output.push('\n');
                }
                if let Some(valid_to) = &state.valid_to {
                    output.push_str("    valid_to: ");
                    output.push_str(&Self::yaml_scalar(valid_to));
                    output.push('\n');
                }
            }
        }

        for (key, value) in [
            ("memory_kind", request.memory_kind.as_deref()),
            ("task_status", request.task_status.as_deref()),
            ("reminder_status", request.reminder_status.as_deref()),
            ("due_at", request.due_at.as_deref()),
            ("source_type", request.source_type.as_deref()),
            ("source_id", request.source_id.as_deref()),
            ("source_path", request.source_path.as_deref()),
            ("source_anchor", request.source_anchor.as_deref()),
            ("source_excerpt", request.source_excerpt.as_deref()),
        ] {
            if let Some(value) = value {
                output.push_str(key);
                output.push_str(": ");
                output.push_str(&Self::yaml_scalar(value));
                output.push('\n');
            }
        }

        output.push_str("---\n\n");

        if !request.title.trim().is_empty() {
            output.push_str("# ");
            output.push_str(request.title.trim());
            output.push_str("\n\n");
        }

        output.push_str(request.body.trim());
        output.push('\n');

        output
    }

    pub fn supersede_state_in_file(
        &self,
        path: &str,
        entity: &str,
        state: &str,
        valid_to: &str,
    ) -> Result<()> {
        if !path.starts_with("journal/") || !path.ends_with(".md") {
            return Err(
                "Memory state supersession path must be a Markdown file inside journal/.".into(),
            );
        }
        if entity.trim().is_empty() || state.trim().is_empty() {
            return Err("Memory state supersession requires non-empty entity and state.".into());
        }
        let valid_to = valid_to.trim();
        if valid_to.len() != 10
            || valid_to.as_bytes()[4] != b'-'
            || valid_to.as_bytes()[7] != b'-'
            || !valid_to
                .bytes()
                .enumerate()
                .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        {
            return Err("Memory state supersession requires a YYYY-MM-DD valid_to date.".into());
        }

        let destination = self.resolve_path(path)?;
        let original = fs::read_to_string(&destination)?;
        let had_trailing_newline = original.ends_with('\n');
        let mut lines = original.lines().map(str::to_string).collect::<Vec<_>>();

        if lines.first().map(String::as_str) != Some("---") {
            return Err("Memory state supersession requires Markdown frontmatter.".into());
        }

        let frontmatter_end = lines
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, line)| (line.trim() == "---").then_some(index))
            .ok_or("Memory state supersession requires a closed frontmatter block.")?;

        let states_header = lines
            .iter()
            .enumerate()
            .skip(1)
            .take(frontmatter_end.saturating_sub(1))
            .find_map(|(index, line)| (line.trim() == "states:").then_some(index))
            .ok_or("Memory file does not contain a states section.")?;

        fn scalar_value(value: &str) -> Result<String> {
            let value = value.trim();
            if value.starts_with('"') {
                Ok(serde_json::from_str::<String>(value)?)
            } else {
                Ok(value.to_string())
            }
        }

        let state_starts = lines
            .iter()
            .enumerate()
            .skip(states_header + 1)
            .take(frontmatter_end.saturating_sub(states_header + 1))
            .filter_map(|(index, line)| line.starts_with("  - entity:").then_some(index))
            .collect::<Vec<_>>();

        let states_section_end = lines
            .iter()
            .enumerate()
            .skip(states_header + 1)
            .take(frontmatter_end.saturating_sub(states_header + 1))
            .find_map(|(index, line)| {
                (index > states_header + 1
                    && !line.trim().is_empty()
                    && line
                        .chars()
                        .next()
                        .is_some_and(|character| !character.is_whitespace()))
                .then_some(index)
            })
            .unwrap_or(frontmatter_end);

        let mut matching_state = None;
        for (offset, start) in state_starts.iter().copied().enumerate() {
            let end = state_starts
                .get(offset + 1)
                .copied()
                .unwrap_or(states_section_end);
            let entity_value = scalar_value(
                lines[start]
                    .strip_prefix("  - entity:")
                    .ok_or("Invalid states section entity line.")?,
            )?;
            let state_value = scalar_value(
                lines
                    .get(start + 1)
                    .and_then(|line| line.strip_prefix("    state:"))
                    .ok_or("Invalid states section state line.")?,
            )?;

            if entity_value == entity.trim() && state_value == state.trim() {
                if matching_state.is_some() {
                    return Err(
                        "Memory state supersession matched multiple identical state entries."
                            .into(),
                    );
                }
                matching_state = Some((start, end));
            }
        }

        let Some((start, end)) = matching_state else {
            return Err(format!(
                "Memory state supersession could not find state for entity '{}' with state '{}'.",
                entity.trim(),
                state.trim()
            )
            .into());
        };

        for index in (start + 2)..end {
            if let Some(value) = lines[index].strip_prefix("    valid_to:") {
                let existing = scalar_value(value)?;
                if existing == valid_to {
                    return Ok(());
                }
                return Err(format!(
                    "Memory state already has valid_to '{}', refusing to replace it with '{}'.",
                    existing, valid_to
                )
                .into());
            }
        }

        lines.insert(
            end,
            format!("    valid_to: {}", Self::yaml_scalar(valid_to)),
        );

        let mut updated = lines.join("\n");
        if had_trailing_newline {
            updated.push('\n');
        }

        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let temp = destination.with_file_name(format!(
            ".{}.{}.tmp",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("memory"),
            timestamp
        ));
        fs::write(&temp, &updated)?;

        let latest = fs::read_to_string(&destination)?;
        if latest != original {
            let _ = fs::remove_file(&temp);
            return Err(format!("Memory state file changed concurrently: {}", path).into());
        }

        if let Err(error) = fs::rename(&temp, &destination) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }

        Ok(())
    }

    pub fn write_request_idempotent(
        &self,
        request: &MemoryWriteRequest,
    ) -> Result<crate::ToolResult> {
        if request.path.trim().is_empty() {
            return Err("memory.write requires a path.".into());
        }
        if !request.path.starts_with("journal/") || !request.path.ends_with(".md") {
            return Err("memory.write path must be a Markdown file inside journal/.".into());
        }
        if request.note_type != "journal" {
            return Err("memory.write type must be 'journal'.".into());
        }
        if request.title.trim().is_empty() {
            return Err("memory.write requires a title.".into());
        }
        if request.body.trim().is_empty() {
            return Err("memory.write requires non-empty body.".into());
        }
        if request.tags.len() > 8 {
            return Err("memory.write accepts at most 8 tags.".into());
        }
        for tag in &request.tags {
            if tag.trim().is_empty() || tag.chars().count() > 64 || tag.contains(['\n', '\r']) {
                return Err(
                    "memory.write tags must be non-empty, single-line, and at most 64 characters."
                        .into(),
                );
            }
        }
        request.validate_structured_metadata()?;

        let destination = self.resolve_path(&request.path)?;
        let rendered = Self::render(request);

        if destination.exists() {
            let existing = fs::read_to_string(&destination)?;
            if existing == rendered {
                return Ok(crate::ToolResult {
                    success: true,
                    output: serde_json::json!({
                        "path": request.path,
                        "created": false,
                        "already_present": true
                    }),
                });
            }
            return Err(format!(
                "Memory file already exists with different content: {}",
                request.path
            )
            .into());
        }

        let parent = destination
            .parent()
            .ok_or("Memory destination has no parent directory.")?;
        fs::create_dir_all(parent)?;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let temp = destination.with_file_name(format!(
            ".{}.{}.tmp",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("memory"),
            timestamp
        ));

        fs::write(&temp, &rendered)?;

        match fs::hard_link(&temp, &destination) {
            Ok(()) => {
                fs::remove_file(&temp)?;
            }
            Err(_error) if destination.exists() => {
                let existing = fs::read_to_string(&destination)?;
                let _ = fs::remove_file(&temp);
                if existing == rendered {
                    return Ok(crate::ToolResult {
                        success: true,
                        output: serde_json::json!({
                            "path": request.path,
                            "created": false,
                            "already_present": true
                        }),
                    });
                }
                return Err(format!(
                    "Memory file already exists with different content: {}",
                    request.path
                )
                .into());
            }
            Err(error) => {
                let _ = fs::remove_file(&temp);
                return Err(error.into());
            }
        }

        Ok(crate::ToolResult {
            success: true,
            output: serde_json::json!({
                "path": request.path,
                "created": true
            }),
        })
    }
}

impl crate::Tool for MemoryWriteTool {
    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition {
            name:
                "memory.write".to_string(),
            description:
                "Create a new Markdown memory file inside the configured memory root. Existing files are never overwritten. Optional structured entities, relationships, events, states, task/reminder status, and due times may be recorded when directly supported by the memory."
                    .to_string(),
            permission:
                crate::ToolPermission::ApprovalRequired,
        }
    }

    fn execute(&self, arguments: &Value) -> crate::Result<crate::ToolResult> {
        let request: MemoryWriteRequest = serde_json::from_value(arguments.clone())?;

        if request.path.trim().is_empty() {
            return Err("memory.write requires a path.".into());
        }

        if !request.path.starts_with("journal/") || !request.path.ends_with(".md") {
            return Err("memory.write path must be a Markdown file inside journal/.".into());
        }

        if request.note_type != "journal" {
            return Err("memory.write type must be 'journal'.".into());
        }

        if request.title.trim().is_empty() {
            return Err("memory.write requires a title.".into());
        }

        if request.note_type.trim().is_empty() {
            return Err("memory.write requires a type.".into());
        }

        if request.body.trim().is_empty() {
            return Err("memory.write requires non-empty body.".into());
        }

        if request.tags.len() > 8 {
            return Err("memory.write accepts at most 8 tags.".into());
        }

        for tag in &request.tags {
            if tag.trim().is_empty() || tag.chars().count() > 64 || tag.contains(['\n', '\r']) {
                return Err(
                    "memory.write tags must be non-empty, single-line, and at most 64 characters."
                        .into(),
                );
            }
        }

        request.validate_structured_metadata()?;

        let destination = self.resolve_new_file(&request.path)?;

        let rendered = Self::render(&request);

        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();

        let temp = destination.with_file_name(format!(
            ".{}.{}.tmp",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("memory"),
            timestamp
        ));

        fs::write(&temp, rendered)?;

        fs::rename(&temp, &destination)?;

        Ok(crate::ToolResult {
            success: true,
            output: serde_json::json!({
                "path": request.path,
                "created": true
            }),
        })
    }
}
