use crate::model::{EntityRecord, EventRecord, NoteMeta, RelationshipRecord, StateRecord};

pub struct ParsedMarkdown {
    pub metadata: NoteMeta,
    pub body: String,
    pub had_frontmatter: bool,

    pub entities: Vec<EntityRecord>,
    pub relationships: Vec<RelationshipRecord>,
    pub events: Vec<EventRecord>,
    pub states: Vec<StateRecord>,
}

fn strip_quotes(value: &str) -> String {
    let value = value.trim();

    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];

        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }

    value.to_string()
}

fn parse_bool(value: &str) -> Option<bool> {
    match strip_quotes(value).to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn apply_field(meta: &mut NoteMeta, key: &str, value: &str) {
    match key {
        "id" => meta.id = Some(strip_quotes(value)),
        "title" => meta.title = Some(strip_quotes(value)),
        "note_type" => meta.note_type = Some(strip_quotes(value)),
        "type" => meta.note_type = Some(strip_quotes(value)),
        "namespace" => meta.namespace = Some(strip_quotes(value)),
        "canonical" => meta.canonical = parse_bool(value),
        "status" => meta.status = Some(strip_quotes(value)),
        "memory_kind" => meta.memory_kind = Some(strip_quotes(value)),
        "task_status" => meta.task_status = Some(strip_quotes(value)),
        "reminder_status" => meta.reminder_status = Some(strip_quotes(value)),
        "calendar_sync_enabled" => meta.calendar_sync_enabled = parse_bool(value),
        "google_calendar_id" => meta.google_calendar_id = Some(strip_quotes(value)),
        "google_event_id" => meta.google_event_id = Some(strip_quotes(value)),
        "calendar_sync_status" => meta.calendar_sync_status = Some(strip_quotes(value)),
        "calendar_last_synced_at" => meta.calendar_last_synced_at = Some(strip_quotes(value)),
        "calendar_sync_error" => meta.calendar_sync_error = Some(strip_quotes(value)),

        "captured_at" => meta.captured_at = Some(strip_quotes(value)),
        "occurred_at" => meta.occurred_at = Some(strip_quotes(value)),
        "valid_from" => meta.valid_from = Some(strip_quotes(value)),
        "valid_to" => meta.valid_to = Some(strip_quotes(value)),
        "due_at" => meta.due_at = Some(strip_quotes(value)),
        "updated_at" => meta.updated_at = Some(strip_quotes(value)),
        "created_at" => meta.created_at = Some(strip_quotes(value)),

        _ => {}
    }
}

pub fn parse(content: &str) -> ParsedMarkdown {
    let mut lines = content.lines();

    let Some(first_line) = lines.next() else {
        return ParsedMarkdown {
            metadata: NoteMeta::default(),
            body: String::new(),
            had_frontmatter: false,
            entities: Vec::new(),
            relationships: Vec::new(),
            events: Vec::new(),
            states: Vec::new(),
        };
    };

    if first_line.trim() != "---" {
        return ParsedMarkdown {
            metadata: NoteMeta::default(),
            body: content.to_string(),
            had_frontmatter: false,
            entities: Vec::new(),
            relationships: Vec::new(),
            events: Vec::new(),
            states: Vec::new(),
        };
    }

    let mut metadata = NoteMeta::default();
    let mut entities = Vec::new();
    let mut relationships = Vec::new();
    let mut events = Vec::new();
    let mut states = Vec::new();

    let mut section = "";
    let mut current_entity: Option<usize> = None;
    let mut current_relationship: Option<usize> = None;
    let mut current_event: Option<usize> = None;
    let mut current_state: Option<usize> = None;
    let mut event_entities_mode = false;

    let mut found_end = false;
    let mut body_start_offset = 0usize;
    let mut consumed = first_line.len();

    for line in &mut lines {
        consumed += 1 + line.len();

        if line.trim() == "---" {
            found_end = true;
            body_start_offset = consumed;
            break;
        }

        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        // Our frontmatter grammar uses indentation to distinguish
        // top-level fields from nested structured fields.
        let top_level = line == line.trim_start();

        // Top-level scalar metadata is always interpreted as metadata,
        // even if a structured section appeared earlier in the document.
        if top_level {
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();

                match key {
                    "id"
                    | "title"
                    | "note_type"
                    | "type"
                    | "namespace"
                    | "canonical"
                    | "status"
                    | "memory_kind"
                    | "task_status"
                    | "reminder_status"
                    | "calendar_sync_enabled"
                    | "google_calendar_id"
                    | "google_event_id"
                    | "calendar_sync_status"
                    | "calendar_last_synced_at"
                    | "calendar_sync_error"
                    | "captured_at"
                    | "occurred_at"
                    | "valid_from"
                    | "valid_to"
                    | "due_at"
                    | "updated_at"
                    | "created_at" => {
                        apply_field(&mut metadata, key, value.trim());
                        continue;
                    }
                    _ => {}
                }
            }

            if trimmed == "entities:" {
                section = "entities";
                current_entity = None;
                current_relationship = None;
                current_event = None;
                current_state = None;
                event_entities_mode = false;
                continue;
            }

            if trimmed == "relationships:" {
                section = "relationships";
                current_entity = None;
                current_relationship = None;
                current_event = None;
                current_state = None;
                event_entities_mode = false;
                continue;
            }

            if trimmed == "events:" {
                section = "events";
                current_entity = None;
                current_relationship = None;
                current_event = None;
                current_state = None;
                event_entities_mode = false;
                continue;
            }

            if trimmed == "states:" {
                section = "states";
                current_entity = None;
                current_relationship = None;
                current_event = None;
                current_state = None;
                event_entities_mode = false;
                continue;
            }
        }

        match section {
            "entities" => {
                if let Some(value) = trimmed.strip_prefix("- name:") {
                    entities.push(EntityRecord {
                        name: strip_quotes(value.trim()),
                        entity_type: "unknown".to_string(),
                    });
                    current_entity = Some(entities.len() - 1);
                    continue;
                }

                if let Some(value) = trimmed.strip_prefix("type:") {
                    if let Some(index) = current_entity {
                        entities[index].entity_type = strip_quotes(value.trim());
                    }
                    continue;
                }
            }

            "relationships" => {
                if let Some(value) = trimmed.strip_prefix("- source:") {
                    relationships.push(RelationshipRecord {
                        source: strip_quotes(value.trim()),
                        target: String::new(),
                        relationship: String::new(),
                    });
                    current_relationship = Some(relationships.len() - 1);
                    continue;
                }

                if let Some(value) = trimmed.strip_prefix("target:") {
                    if let Some(index) = current_relationship {
                        relationships[index].target = strip_quotes(value.trim());
                    }
                    continue;
                }

                if let Some(value) = trimmed.strip_prefix("relationship:") {
                    if let Some(index) = current_relationship {
                        relationships[index].relationship = strip_quotes(value.trim());
                    }
                    continue;
                }
            }

            "events" => {
                if let Some(value) = trimmed.strip_prefix("- type:") {
                    events.push(EventRecord {
                        event_type: strip_quotes(value.trim()),
                        occurred_at: None,
                        description: None,
                        entities: Vec::new(),
                    });
                    current_event = Some(events.len() - 1);
                    event_entities_mode = false;
                    continue;
                }

                let Some(index) = current_event else {
                    continue;
                };

                // Nested event entity list.
                if trimmed == "entities:" {
                    event_entities_mode = true;
                    continue;
                }

                if event_entities_mode {
                    if let Some(value) = trimmed.strip_prefix("- ") {
                        events[index].entities.push(strip_quotes(value.trim()));
                        continue;
                    }
                }

                if let Some(value) = trimmed.strip_prefix("occurred_at:") {
                    events[index].occurred_at = Some(strip_quotes(value.trim()));
                    event_entities_mode = false;
                    continue;
                }

                if let Some(value) = trimmed.strip_prefix("description:") {
                    events[index].description = Some(strip_quotes(value.trim()));
                    event_entities_mode = false;
                    continue;
                }
            }

            "states" => {
                if let Some(value) = trimmed.strip_prefix("- entity:") {
                    states.push(StateRecord {
                        entity: strip_quotes(value.trim()),
                        state: String::new(),
                        valid_from: None,
                        valid_to: None,
                    });
                    current_state = Some(states.len() - 1);
                    continue;
                }

                let Some(index) = current_state else {
                    continue;
                };

                if let Some(value) = trimmed.strip_prefix("state:") {
                    states[index].state = strip_quotes(value.trim());
                    continue;
                }

                if let Some(value) = trimmed.strip_prefix("valid_from:") {
                    states[index].valid_from = Some(strip_quotes(value.trim()));
                    continue;
                }

                if let Some(value) = trimmed.strip_prefix("valid_to:") {
                    states[index].valid_to = Some(strip_quotes(value.trim()));
                    continue;
                }
            }

            _ => {}
        }
    }

    if !found_end {
        return ParsedMarkdown {
            metadata: NoteMeta::default(),
            body: content.to_string(),
            had_frontmatter: false,
            entities: Vec::new(),
            relationships: Vec::new(),
            events: Vec::new(),
            states: Vec::new(),
        };
    }

    let body = content
        .get(body_start_offset..)
        .unwrap_or_default()
        .trim_start_matches('\n')
        .to_string();

    ParsedMarkdown {
        metadata,
        body,
        had_frontmatter: true,
        entities,
        relationships,
        events,
        states,
    }
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn add_missing_frontmatter(
    content: &str,
    path: &str,
    default_title: &str,
    default_note_type: &str,
    default_namespace: &str,
    default_canonical: bool,
    now: &str,
) -> (String, bool) {
    let parsed = parse(content);

    let id = parsed
        .metadata
        .id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let title = parsed
        .metadata
        .title
        .clone()
        .unwrap_or_else(|| default_title.to_string());

    let note_type = parsed
        .metadata
        .note_type
        .clone()
        .unwrap_or_else(|| default_note_type.to_string());

    let namespace = parsed
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| default_namespace.to_string());

    let canonical = parsed.metadata.canonical.unwrap_or(default_canonical);

    let status = parsed
        .metadata
        .status
        .clone()
        .unwrap_or_else(|| "active".to_string());

    let created_at = parsed
        .metadata
        .created_at
        .clone()
        .unwrap_or_else(|| now.to_string());

    let updated_at = parsed
        .metadata
        .updated_at
        .clone()
        .unwrap_or_else(|| now.to_string());

    let mut missing_fields = Vec::new();

    if parsed.metadata.id.is_none() {
        missing_fields.push(format!("id: {}", yaml_string(&id)));
    }

    if parsed.metadata.title.is_none() {
        missing_fields.push(format!("title: {}", yaml_string(&title)));
    }

    if parsed.metadata.note_type.is_none() {
        missing_fields.push(format!("note_type: {}", yaml_string(&note_type)));
    }

    if parsed.metadata.namespace.is_none() {
        missing_fields.push(format!("namespace: {}", yaml_string(&namespace)));
    }

    if parsed.metadata.canonical.is_none() {
        missing_fields.push(format!("canonical: {canonical}"));
    }

    if parsed.metadata.status.is_none() {
        missing_fields.push(format!("status: {}", yaml_string(&status)));
    }

    if parsed.metadata.created_at.is_none() {
        missing_fields.push(format!("created_at: {}", yaml_string(&created_at)));
    }

    if parsed.metadata.updated_at.is_none() {
        missing_fields.push(format!("updated_at: {}", yaml_string(&updated_at)));
    }

    if missing_fields.is_empty() {
        return (content.to_string(), false);
    }

    if parsed.had_frontmatter {
        let mut output = String::new();

        let mut lines = content.lines();

        if let Some(first) = lines.next() {
            output.push_str(first);
            output.push('\n');
        }

        let mut inserted = false;

        for line in lines {
            if line.trim() == "---" && !inserted {
                for field in &missing_fields {
                    output.push_str(field);
                    output.push('\n');
                }

                inserted = true;
            }

            output.push_str(line);
            output.push('\n');
        }

        return (output.trim_end_matches('\n').to_string() + "\n", true);
    }

    let mut output = String::new();

    output.push_str("---\n");
    output.push_str(&format!("id: {}\n", yaml_string(&id)));
    output.push_str(&format!("title: {}\n", yaml_string(&title)));
    output.push_str(&format!("note_type: {}\n", yaml_string(&note_type)));
    output.push_str(&format!("namespace: {}\n", yaml_string(&namespace)));
    output.push_str(&format!("canonical: {canonical}\n"));
    output.push_str(&format!("status: {}\n", yaml_string(&status)));
    output.push_str(&format!("created_at: {}\n", yaml_string(&created_at)));
    output.push_str(&format!("updated_at: {}\n", yaml_string(&updated_at)));
    output.push_str("---\n\n");

    output.push_str(content);

    if !content.ends_with('\n') {
        output.push('\n');
    }

    let _ = path;

    (output, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_task_and_reminder_metadata() {
        let task = parse(
            r#"---
memory_kind: task
task_status: in_progress
due_at: 2026-09-20T10:00:00Z
---
# Task
Do it."#,
        );
        assert_eq!(task.metadata.memory_kind.as_deref(), Some("task"));
        assert_eq!(task.metadata.task_status.as_deref(), Some("in_progress"));
        assert_eq!(
            task.metadata.due_at.as_deref(),
            Some("2026-09-20T10:00:00Z")
        );

        let reminder = parse(
            r#"---
memory_kind: reminder
reminder_status: scheduled
due_at: 2026-09-21T09:00:00Z
---
# Reminder
Ping."#,
        );
        assert_eq!(reminder.metadata.memory_kind.as_deref(), Some("reminder"));
        assert_eq!(
            reminder.metadata.reminder_status.as_deref(),
            Some("scheduled")
        );

        let synced = parse(
            r#"---
memory_kind: reminder
reminder_status: scheduled
calendar_sync_enabled: true
google_calendar_id: primary
google_event_id: event-123
calendar_sync_status: synced
calendar_last_synced_at: 2026-09-22T10:00:00Z
calendar_sync_error: ""
---
Call Mom."#,
        );
        assert_eq!(synced.metadata.calendar_sync_enabled, Some(true));
        assert_eq!(
            synced.metadata.google_calendar_id.as_deref(),
            Some("primary")
        );
        assert_eq!(
            synced.metadata.google_event_id.as_deref(),
            Some("event-123")
        );
        assert_eq!(
            synced.metadata.calendar_sync_status.as_deref(),
            Some("synced")
        );
        assert_eq!(
            synced.metadata.calendar_last_synced_at.as_deref(),
            Some("2026-09-22T10:00:00Z")
        );
        assert_eq!(synced.metadata.calendar_sync_error.as_deref(), Some(""));
    }

    #[test]
    fn parses_frontmatter() {
        let input = r#"---
id: abc
title: "Local AI"
note_type: canonical
namespace: topic
canonical: true
status: active
occurred_at: 2026-09-04T11:10:00Z
---

# Local AI

Test.
"#;

        let parsed = parse(input);

        assert!(parsed.had_frontmatter);
        assert_eq!(parsed.metadata.id.as_deref(), Some("abc"));
        assert_eq!(parsed.metadata.title.as_deref(), Some("Local AI"));
        assert_eq!(parsed.metadata.note_type.as_deref(), Some("canonical"));
        assert_eq!(parsed.metadata.namespace.as_deref(), Some("topic"));
        assert_eq!(parsed.metadata.canonical, Some(true));
        assert!(parsed.body.contains("# Local AI"));
    }

    #[test]
    fn parses_structured_frontmatter() {
        let input = r#"---
title: Local AI
note_type: journal
namespace: topic
canonical: false
status: active

entities:
  - name: Qwen3.5
    type: technology
  - name: NVIDIA GTX 1650
    type: hardware

relationships:
  - source: Qwen3.5
    target: llama.cpp
    relationship: runs_on

events:
  - type: decision
    occurred_at: 2026-09-03T19:00:00Z
    description: Selected Qwen3.5 for evaluation.
    entities:
      - Qwen3.5

states:
  - entity: Qwen3.5
    state: Selected for evaluation
    valid_from: 2026-09-03
    valid_to: 2026-09-10
---

# Local AI
"#;

        let parsed = parse(input);

        assert_eq!(parsed.entities.len(), 2);
        assert_eq!(parsed.entities[0].name, "Qwen3.5");
        assert_eq!(parsed.entities[0].entity_type, "technology");

        assert_eq!(parsed.relationships.len(), 1);
        assert_eq!(parsed.relationships[0].relationship, "runs_on");

        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].event_type, "decision");
        assert_eq!(parsed.events[0].entities, vec!["Qwen3.5"]);

        assert_eq!(parsed.states.len(), 1);
        assert_eq!(parsed.states[0].state, "Selected for evaluation");
        assert_eq!(parsed.states[0].valid_from.as_deref(), Some("2026-09-03"));
    }

    #[test]
    fn parses_top_level_fields_after_structured_sections() {
        let input = r#"---
title: Legacy Note
type: journal

entities:
  - name: Qwen3.5
    type: technology

events:
  - type: decision
    occurred_at: 2026-09-03T19:00:00Z
    description: Selected the model.
    entities:
      - Qwen3.5

states:
  - entity: Qwen3.5
    state: Selected
    valid_from: 2026-09-03

id: "stable-id"
namespace: "topic"
canonical: false
status: active
created_at: "2026-09-03T19:00:00Z"
updated_at: "2026-09-04T19:00:00Z"
---

# Legacy Note
"#;

        let parsed = parse(input);

        assert_eq!(parsed.metadata.id.as_deref(), Some("stable-id"));
        assert_eq!(parsed.metadata.note_type.as_deref(), Some("journal"));
        assert_eq!(parsed.metadata.namespace.as_deref(), Some("topic"));
        assert_eq!(parsed.metadata.canonical, Some(false));

        assert_eq!(parsed.entities.len(), 1);
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].entities, vec!["Qwen3.5"]);
        assert_eq!(parsed.states.len(), 1);
    }

    #[test]
    fn parses_plain_markdown() {
        let input = "# Hello\n\nWorld.";

        let parsed = parse(input);

        assert!(!parsed.had_frontmatter);
        assert_eq!(parsed.body, input);
    }

    #[test]
    fn adds_frontmatter_to_plain_markdown() {
        let input = "# Hello\n\nWorld.";

        let (output, changed) = add_missing_frontmatter(
            input,
            "topics/hello.md",
            "Hello",
            "canonical",
            "topic",
            true,
            "2026-09-10T00:00:00Z",
        );

        assert!(changed);
        assert!(output.starts_with("---\n"));
        assert!(output.contains("title: \"Hello\""));
        assert!(output.contains("# Hello"));
    }

    #[test]
    fn preserves_existing_unknown_frontmatter() {
        let input = r#"---
title: Hello
custom_field: preserve-me
---

Body.
"#;

        let (output, changed) = add_missing_frontmatter(
            input,
            "topics/hello.md",
            "Hello",
            "canonical",
            "topic",
            true,
            "2026-09-10T00:00:00Z",
        );

        assert!(changed);
        assert!(output.contains("custom_field: preserve-me"));
        assert!(output.contains("id:"));
    }
}
