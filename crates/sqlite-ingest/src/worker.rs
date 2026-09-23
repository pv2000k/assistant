use orchestrator::{ModelCall, ModelExecutor, ModelTarget};
use serde::{Deserialize, Serialize};
use sqlite_memory::SqliteMemoryDb;
use std::error::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tools::{
    MemoryWriteEntity, MemoryWriteEvent, MemoryWriteRelationship, MemoryWriteRequest,
    MemoryWriteState,
};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

const EXTRACT_MEMORY_JOB: &str = "extract_memory";
const CONVERSATION_TARGET: &str = "conversation";
const MAX_ITEMS: usize = 4;
const MAX_ITEM_BODY_CHARS: usize = 4000;
const RETRY_DELAY_SECONDS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionDecision {
    MustSave,
    ShouldSave,
    DontSave,
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionProposal {
    decision: ExtractionDecision,
    #[serde(default)]
    items: Vec<ExtractionItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionItem {
    kind: String,
    title: String,
    body: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    entities: Vec<MemoryWriteEntity>,
    #[serde(default)]
    relationships: Vec<MemoryWriteRelationship>,
    #[serde(default)]
    events: Vec<MemoryWriteEvent>,
    #[serde(default)]
    states: Vec<MemoryWriteState>,
    #[serde(default)]
    task_status: Option<String>,
    #[serde(default)]
    reminder_status: Option<String>,
    #[serde(default)]
    due_at: Option<String>,
}

pub struct MemoryExtractionWorker<M> {
    db: SqliteMemoryDb,
    models: M,
    auto_apply_indexer: Option<orchestrator::PersistentMemoryIndexer>,
}

impl<M> MemoryExtractionWorker<M>
where
    M: ModelExecutor,
{
    pub fn new(db: SqliteMemoryDb, models: M) -> Self {
        Self {
            db,
            models,
            auto_apply_indexer: None,
        }
    }

    pub fn with_auto_apply_indexer(
        mut self,
        indexer: orchestrator::PersistentMemoryIndexer,
    ) -> Self {
        self.auto_apply_indexer = Some(indexer);
        self
    }

    pub fn run_once(&self) -> Result<()> {
        let now = now_timestamp();
        self.db
            .recover_running_jobs_of_type(&now, EXTRACT_MEMORY_JOB)?;

        let Some(job) = self.db.claim_next_job_of_type(&now, EXTRACT_MEMORY_JOB)? else {
            return Ok(());
        };

        match self.process_job(&job) {
            Ok(()) => self.db.complete_job(&job.id),
            Err(error) => {
                self.db.fail_job(
                    &job.id,
                    &truncate_error(&error.to_string()),
                    &retry_timestamp()?,
                )?;
                Err(error)
            }
        }
    }

    fn process_job(&self, job: &sqlite_memory::Job) -> Result<()> {
        if job.target_type.as_deref() != Some(CONVERSATION_TARGET) {
            return Err("extract_memory job has an invalid target_type.".into());
        }

        let target_id = job
            .target_id
            .as_deref()
            .ok_or("extract_memory job is missing its conversation target_id.")?;
        let turn_id = target_id.parse::<i64>()?;

        self.process_turn(turn_id)
    }

    fn process_turn(&self, turn_id: i64) -> Result<()> {
        if let Some(existing) = self
            .db
            .latest_memory_extraction_proposal_for_turn(turn_id)?
        {
            if existing.decision == "must_save" && existing.status == "pending" {
                if let Some(indexer) = &self.auto_apply_indexer {
                    indexer.apply_memory_extraction_proposal(&existing.id)?;
                }
            }
            return Ok(());
        }

        let turn = self
            .db
            .conversation_turn(turn_id)?
            .ok_or("conversation turn referenced by extraction job was not found.")?;

        let call = ModelCall {
            target: ModelTarget::Qwen,
            prompt: extraction_prompt(&turn.user_text, &turn.assistant_text),
        };

        let response_schema = extraction_response_schema();
        let output = self
            .models
            .execute_with_response_format(&call, Some(&response_schema))?;

        let proposal =
            enforce_save_policy(parse_extraction_output(&output.output)?, &turn.user_text);
        let payload_json = serde_json::to_string(&proposal)?;

        let proposal_id = self.db.store_memory_extraction_proposal(
            turn_id,
            decision_name(proposal.decision),
            &payload_json,
        )?;

        if matches!(
            proposal.decision,
            ExtractionDecision::MustSave | ExtractionDecision::ShouldSave
        ) {
            if let Some(indexer) = &self.auto_apply_indexer {
                indexer.apply_memory_extraction_proposal(&proposal_id)?;
            }
        }

        Ok(())
    }
}

fn extraction_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["decision", "items"],
        "properties": {
            "decision": {
                "enum": ["must_save", "should_save", "dont_save", "uncertain"]
            },
            "items": {
                "type": "array",
                "maxItems": MAX_ITEMS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "kind",
                        "title",
                        "body",
                        "tags",
                        "entities",
                        "relationships",
                        "events",
                        "states",
                        "task_status",
                        "reminder_status",
                        "due_at"
                    ],
                    "properties": {
                        "kind": {
                            "enum": [
                                "fact",
                                "thought",
                                "idea",
                                "decision",
                                "event",
                                "state",
                                "preference",
                                "relationship",
                                "task",
                                "reminder",
                                "resource",
                                "conversation_derived"
                            ]
                        },
                        "title": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 256
                        },
                        "body": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MAX_ITEM_BODY_CHARS
                        },
                        "tags": {
                            "type": "array",
                            "maxItems": 8,
                            "items": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 64
                            }
                        },
                        "entities": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["name", "type"],
                                "properties": {
                                    "name": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 256
                                    },
                                    "type": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 64
                                    }
                                }
                            }
                        },
                        "relationships": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["source", "target", "relationship"],
                                "properties": {
                                    "source": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 256
                                    },
                                    "target": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 256
                                    },
                                    "relationship": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 128
                                    }
                                }
                            }
                        },
                        "events": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": [
                                    "type",
                                    "occurred_at",
                                    "description",
                                    "entities"
                                ],
                                "properties": {
                                    "type": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 64
                                    },
                                    "occurred_at": {
                                        "type": ["string", "null"]
                                    },
                                    "description": {
                                        "type": ["string", "null"]
                                    },
                                    "entities": {
                                        "type": "array",
                                        "items": {
                                            "type": "string"
                                        }
                                    }
                                }
                            }
                        },
                        "task_status": {
                            "type": ["string", "null"],
                            "enum": ["open", "in_progress", "completed", "cancelled", "expired", "archived", null]
                        },
                        "reminder_status": {
                            "type": ["string", "null"],
                            "enum": ["scheduled", "triggered", "completed", "cancelled", null]
                        },
                        "due_at": {
                            "type": ["string", "null"],
                            "maxLength": 64
                        },
                        "states": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": [
                                    "entity",
                                    "state",
                                    "valid_from",
                                    "valid_to"
                                ],
                                "properties": {
                                    "entity": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 256
                                    },
                                    "state": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 256
                                    },
                                    "valid_from": {
                                        "type": ["string", "null"]
                                    },
                                    "valid_to": {
                                        "type": ["string", "null"]
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

fn truncate_parse_output(output: &str) -> String {
    let mut truncated = output
        .chars()
        .take(MAX_PARSE_ERROR_OUTPUT_CHARS)
        .collect::<String>();
    if output.chars().count() > MAX_PARSE_ERROR_OUTPUT_CHARS {
        truncated.push_str("...<truncated>");
    }
    truncated
}

fn extraction_prompt(user_text: &str, assistant_text: &str) -> String {
    format!(
        r#"You are the background memory extraction stage of a personal AI assistant.
Return ONLY valid JSON. Do not use Markdown fences.

Extract only durable, useful information from the USER MESSAGE. Prefer user-authored facts,
preferences, decisions, tasks, reminders, events, relationships, and meaningful project state.
Treat the assistant response only as context for disambiguation. Do not store claims introduced
only by the assistant. Do not invent facts or infer sensitive details the user did not clearly state.

Decision values:
- must_save: the user explicitly asked to remember, save, store, memorize, or add information to memory.
- should_save: durable information is present but the user did not explicitly request saving.
- dont_save: nothing durable should be stored.
- uncertain: the material is too ambiguous to safely extract.

Use at most {MAX_ITEMS} items. Each item must contain kind, title, body, tags, entities,
relationships, events, and states. Keep each body under {MAX_ITEM_BODY_CHARS} characters.

IMPORTANT STRUCTURED FIELD RULES:
- entities MUST be an array of objects, never strings.
- Each entity object MUST contain exactly:
  {{"name":"...","type":"..."}}
- relationships MUST be an array of objects, never strings.
- Each relationship object MUST contain exactly:
  {{"source":"...","target":"...","relationship":"..."}}
- events MUST be an array of objects.
- Each event object MUST contain exactly:
  {{"type":"...","occurred_at":null,"description":null,"entities":[]}}
- An event's entities MUST be an array of entity names (strings), not entity objects.
- states MUST be an array of objects.
- Each state object MUST contain exactly:
  {{"entity":"...","state":"...","valid_from":null,"valid_to":null}}
- Even when there is no metadata, output the required empty arrays.
- Never replace an object with a string.
- Never omit a required field.
- For kind "task", set task_status to one of open, in_progress, completed, cancelled, expired, or archived.
- For kind "reminder", set reminder_status to one of scheduled, triggered, completed, or cancelled.
- For other kinds, task_status and reminder_status must be null.
- Use due_at only when the user explicitly provides a due or trigger time. Otherwise set it to null.
- Return exactly one JSON object and nothing else.

USER MESSAGE:
{user_text}

ASSISTANT RESPONSE:
{assistant_text}

JSON shape:
{{
  "decision": "must_save|should_save|dont_save|uncertain",
  "items": [
    {{
      "kind": "fact|thought|idea|decision|event|state|preference|relationship|task|reminder|resource|conversation_derived",
      "title": "...",
      "body": "...",
      "tags": [],
      "entities": [
        {{
          "name": "...",
          "type": "..."
        }}
      ],
      "relationships": [
        {{
          "source": "...",
          "target": "...",
          "relationship": "..."
        }}
      ],
      "events": [
        {{
          "type": "...",
          "occurred_at": null,
          "description": null,
          "entities": []
        }}
      ],
      "states": [
        {{
          "entity": "...",
          "state": "...",
          "valid_from": null,
          "valid_to": null
        }}
      ],
      "task_status": null,
      "reminder_status": null,
      "due_at": null
    }}
  ]
}}"#
    )
}

const MAX_PARSE_ERROR_OUTPUT_CHARS: usize = 6000;

fn parse_extraction_output(output: &str) -> Result<ExtractionProposal> {
    let trimmed = output.trim();
    let json = if let Some(stripped) = trimmed.strip_prefix("```") {
        let body = stripped
            .strip_prefix("json")
            .unwrap_or(stripped)
            .trim_start_matches('\n');
        body.strip_suffix("```")
            .ok_or("Extraction output fence is not closed.")?
            .trim()
    } else {
        trimmed
    };

    let proposal: ExtractionProposal = serde_json::from_str(json).map_err(|error| {
        format!(
            "Extraction JSON parse failed: {error}. Raw output (truncated): {}",
            truncate_parse_output(json),
        )
    })?;
    if proposal.items.len() > MAX_ITEMS {
        return Err(format!("Extraction returned more than {MAX_ITEMS} items.").into());
    }

    for item in &proposal.items {
        if !matches!(
            item.kind.trim(),
            "fact"
                | "thought"
                | "idea"
                | "decision"
                | "event"
                | "state"
                | "preference"
                | "relationship"
                | "task"
                | "reminder"
                | "resource"
                | "conversation_derived"
        ) {
            return Err("Extraction item has an unsupported kind.".into());
        }
        if item.kind.trim().is_empty()
            || item.kind.chars().count() > 64
            || item.kind.contains(['\n', '\r'])
            || item.title.trim().is_empty()
            || item.title.chars().count() > 256
            || item.title.contains(['\n', '\r'])
            || item.body.trim().is_empty()
            || item.body.chars().count() > MAX_ITEM_BODY_CHARS
        {
            return Err("Extraction item has invalid kind, title, or body.".into());
        }
        if item.tags.len() > 8 {
            return Err("Extraction item has too many tags.".into());
        }
        for tag in &item.tags {
            if tag.trim().is_empty() || tag.chars().count() > 64 || tag.contains(['\n', '\r']) {
                return Err("Extraction item contains an invalid tag.".into());
            }
        }

        let validation = MemoryWriteRequest {
            path: "journal/extraction.md".to_string(),
            title: item.title.clone(),
            note_type: "journal".to_string(),
            body: item.body.clone(),
            tags: item.tags.clone(),
            entities: item.entities.clone(),
            relationships: item.relationships.clone(),
            events: item.events.clone(),
            states: item.states.clone(),
            memory_kind: Some(item.kind.clone()),
            task_status: item.task_status.clone(),
            reminder_status: item.reminder_status.clone(),
            due_at: item.due_at.clone(),
            source_type: None,
            source_id: None,
            source_path: None,
            source_anchor: None,
            source_excerpt: None,
        };
        validation.validate_structured_metadata()?;

        if item.kind == "task" && item.task_status.is_none() {
            return Err("Task extraction items require task_status.".into());
        }
        if item.kind == "reminder" && item.reminder_status.is_none() {
            return Err("Reminder extraction items require reminder_status.".into());
        }
        if item.kind != "task" && item.task_status.is_some() {
            return Err("Only task extraction items may set task_status.".into());
        }
        if item.kind != "reminder" && item.reminder_status.is_some() {
            return Err("Only reminder extraction items may set reminder_status.".into());
        }
    }

    if proposal.items.is_empty() && proposal.decision != ExtractionDecision::DontSave {
        return Err(
            "Extraction decision requires at least one item unless it is dont_save.".into(),
        );
    }

    Ok(proposal)
}

fn enforce_save_policy(mut proposal: ExtractionProposal, user_text: &str) -> ExtractionProposal {
    let explicit_save = orchestrator::is_explicit_memory_write_request(user_text);

    match proposal.decision {
        ExtractionDecision::MustSave if !explicit_save => {
            proposal.decision = ExtractionDecision::ShouldSave;
        }
        ExtractionDecision::ShouldSave if explicit_save => {
            proposal.decision = ExtractionDecision::MustSave;
        }
        _ => {}
    }

    if proposal.decision == ExtractionDecision::DontSave {
        proposal.items.clear();
    }
    proposal
}

fn decision_name(decision: ExtractionDecision) -> &'static str {
    match decision {
        ExtractionDecision::MustSave => "must_save",
        ExtractionDecision::ShouldSave => "should_save",
        ExtractionDecision::DontSave => "dont_save",
        ExtractionDecision::Uncertain => "uncertain",
    }
}

fn now_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC3339 formatting should never fail")
}

fn retry_timestamp() -> Result<String> {
    Ok(
        (OffsetDateTime::now_utc() + time::Duration::seconds(RETRY_DELAY_SECONDS))
            .format(&Rfc3339)?,
    )
}

fn truncate_error(error: &str) -> String {
    let mut text = error.chars().take(2048).collect::<String>();
    if text.is_empty() {
        text.push_str("Memory extraction failed.");
    }
    text.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Mutex,
        thread,
    };

    struct FakeModel {
        output: String,
        calls: Mutex<usize>,
    }

    impl ModelExecutor for FakeModel {
        fn execute(&self, call: &ModelCall) -> Result<orchestrator::ModelResult> {
            *self.calls.lock().map_err(|_| "calls mutex poisoned.")? += 1;
            Ok(orchestrator::ModelResult {
                target: call.target.clone(),
                output: self.output.clone(),
            })
        }
    }

    fn test_db() -> Result<(tempfile::TempDir, SqliteMemoryDb)> {
        let directory = tempfile::tempdir()?;
        let db = SqliteMemoryDb::open(directory.path().join("worker.db"))?;
        db.initialize_schema()?;
        Ok((directory, db))
    }

    fn start_embedding_server() -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("embedding mock server accept failed");
            let mut request = [0u8; 8192];
            let _ = stream
                .read(&mut request)
                .expect("embedding mock server read failed");
            let embedding = vec![0.0f32; 384];
            let body = serde_json::json!({
                "data": [{"embedding": embedding}]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("embedding mock server write failed");
        });
        Ok((format!("http://{}", address), handle))
    }

    #[test]
    fn extraction_prompt_describes_structured_metadata_shapes() {
        let prompt = extraction_prompt("My current test codename is Bankai.", "Acknowledged.");

        assert!(prompt.contains("entities MUST be an array of objects, never strings."));
        assert!(prompt.contains(
            "An event's entities MUST be an array of entity names (strings), not entity objects."
        ));
        assert!(prompt.contains("\"name\":\"...\",\"type\":\"...\""));
        assert!(prompt.contains("\"source\":\"...\",\"target\":\"...\",\"relationship\":\"...\""));
        assert!(prompt.contains(
            "\"type\":\"...\",\"occurred_at\":null,\"description\":null,\"entities\":[]"
        ));
        assert!(prompt.contains(
            "\"entity\":\"...\",\"state\":\"...\",\"valid_from\":null,\"valid_to\":null"
        ));
    }

    #[test]
    fn extraction_schema_requires_task_and_reminder_metadata_fields() {
        let schema = extraction_response_schema();
        let properties = &schema["properties"]["items"]["items"]["properties"];
        assert!(properties["task_status"].is_object());
        assert!(properties["reminder_status"].is_object());
        assert!(properties["due_at"].is_object());
        assert_eq!(
            schema["properties"]["items"]["items"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|v| matches!(*v, "task_status" | "reminder_status" | "due_at"))
                .count(),
            3
        );
    }

    #[test]
    fn extraction_schema_requires_structured_entities() {
        let schema = extraction_response_schema();

        let entity_schema =
            &schema["properties"]["items"]["items"]["properties"]["entities"]["items"];

        assert_eq!(entity_schema["type"], "object");
        assert_eq!(
            entity_schema["required"],
            serde_json::json!(["name", "type"])
        );
        assert_eq!(
            entity_schema["additionalProperties"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn parses_task_and_reminder_extraction_metadata() -> Result<()> {
        let task = parse_extraction_output(
            r#"{"decision":"should_save","items":[{"kind":"task","title":"Ship it","body":"Ship it.","tags":[],"entities":[],"relationships":[],"events":[],"states":[],"task_status":"open","reminder_status":null,"due_at":"2026-09-20T10:00:00Z"}]}"#,
        )?;
        assert_eq!(task.items[0].task_status.as_deref(), Some("open"));
        assert_eq!(task.items[0].reminder_status, None);
        assert_eq!(
            task.items[0].due_at.as_deref(),
            Some("2026-09-20T10:00:00Z")
        );

        let reminder = parse_extraction_output(
            r#"{"decision":"should_save","items":[{"kind":"reminder","title":"Ping","body":"Ping me.","tags":[],"entities":[],"relationships":[],"events":[],"states":[],"task_status":null,"reminder_status":"scheduled","due_at":"2026-09-21T09:00:00Z"}]}"#,
        )?;
        assert_eq!(
            reminder.items[0].reminder_status.as_deref(),
            Some("scheduled")
        );
        assert_eq!(reminder.items[0].task_status, None);
        Ok(())
    }

    #[test]
    fn parses_event_entities_as_names() -> Result<()> {
        let output = r#"{"decision":"should_save","items":[{"kind":"event","title":"Conversation","body":"User greeted the assistant.","tags":[],"entities":[],"relationships":[],"events":[{"type":"greeting","occurred_at":null,"description":"User greeted the assistant.","entities":["User"]}],"states":[],"task_status":null,"reminder_status":null,"due_at":null}]}"#;
        let proposal = parse_extraction_output(output)?;
        assert_eq!(proposal.items[0].events[0].entities, vec!["User"]);
        Ok(())
    }

    #[test]
    fn parse_error_includes_raw_output_for_schema_mismatch() {
        let output = r#"{"decision":"should_save","items":[{"kind":"fact","title":{"unexpected":"map"},"body":"Something durable.","tags":[],"entities":[],"relationships":[],"events":[],"states":[],"task_status":null,"reminder_status":null,"due_at":null}]}"#;
        let error = parse_extraction_output(output).unwrap_err().to_string();
        assert!(error.contains("Extraction JSON parse failed"));
        assert!(error.contains("invalid type: map, expected a string"));
        assert!(error.contains("\"unexpected\":\"map\""));
    }

    #[test]
    fn parses_valid_extraction_output() -> Result<()> {
        let output = r#"{"decision":"should_save","items":[{"kind":"preference","title":"Local model","body":"Qwen3.5 is preferred for local work.","tags":["local-ai"],"entities":[],"relationships":[],"events":[],"states":[]}] }"#;
        let proposal = parse_extraction_output(output)?;
        assert_eq!(proposal.decision, ExtractionDecision::ShouldSave);
        assert_eq!(proposal.items.len(), 1);
        Ok(())
    }

    #[test]
    fn rejects_unknown_extraction_fields() {
        let output = r#"{"decision":"dont_save","items":[],"extra":true}"#;
        assert!(parse_extraction_output(output).is_err());
    }

    #[test]
    fn must_save_requires_explicit_user_intent() -> Result<()> {
        let output = r#"{"decision":"must_save","items":[{"kind":"fact","title":"Fact","body":"Something durable.","tags":[],"entities":[],"relationships":[],"events":[],"states":[]}] }"#;
        let proposal = parse_extraction_output(output)?;
        assert_eq!(
            enforce_save_policy(proposal, "Here is a statement.").decision,
            ExtractionDecision::ShouldSave
        );
        Ok(())
    }

    #[test]
    fn explicit_save_upgrades_should_save_to_must_save() -> Result<()> {
        let output = r#"{"decision":"should_save","items":[{"kind":"fact","title":"Fact","body":"Something durable.","tags":[],"entities":[],"relationships":[],"events":[],"states":[]}] }"#;
        let proposal = parse_extraction_output(output)?;
        assert_eq!(
            enforce_save_policy(proposal.clone(), "My name is Pranav. Please save that.").decision,
            ExtractionDecision::MustSave
        );
        assert_eq!(
            enforce_save_policy(proposal, "Please remember this: Qwen is my model.").decision,
            ExtractionDecision::MustSave
        );
        Ok(())
    }

    #[test]
    fn explicit_save_preserves_must_save() -> Result<()> {
        let output = r#"{"decision":"must_save","items":[{"kind":"fact","title":"Fact","body":"Something durable.","tags":[],"entities":[],"relationships":[],"events":[],"states":[]}] }"#;
        let proposal = parse_extraction_output(output)?;
        assert_eq!(
            enforce_save_policy(proposal.clone(), "Please remember this: Qwen is my model.")
                .decision,
            ExtractionDecision::MustSave
        );
        assert_eq!(
            enforce_save_policy(proposal, "My name is Pranav. Please save that.").decision,
            ExtractionDecision::MustSave
        );
        Ok(())
    }

    #[test]
    fn worker_processes_job_and_stores_proposal() -> Result<()> {
        let (_directory, db) = test_db()?;
        let turn_id = db.append_conversation_turn(
            "session-1",
            "I prefer Qwen3.5 for local work.",
            "Understood.",
        )?;
        let job_id = db.enqueue_job(
            EXTRACT_MEMORY_JOB,
            Some(CONVERSATION_TARGET),
            Some(&turn_id.to_string()),
            0,
            Some("2026-09-12T10:00:00Z"),
        )?;
        let model = FakeModel {
            output: r#"{"decision":"should_save","items":[{"kind":"preference","title":"Local model preference","body":"Qwen3.5 is preferred for local work.","tags":["local-ai"],"entities":[],"relationships":[],"events":[],"states":[]}] }"#.to_string(),
            calls: Mutex::new(0),
        };
        let worker = MemoryExtractionWorker::new(db, model);
        worker.run_once()?;
        assert_eq!(
            worker.db.job(&job_id)?.unwrap().status,
            sqlite_memory::JobStatus::Completed
        );
        assert_eq!(
            worker
                .db
                .latest_memory_extraction_proposal_for_turn(turn_id)?
                .unwrap()
                .decision,
            "should_save"
        );
        Ok(())
    }

    #[test]
    fn should_save_is_applied_automatically() -> Result<()> {
        let (_directory, db) = test_db()?;
        let database = _directory.path().join("worker.db");
        let memory_root = _directory.path().join("memory");
        std::fs::create_dir_all(&memory_root)?;

        let turn_id = db.append_conversation_turn(
            "session-1",
            "My name is Pranav and I prefer Optimus Prime.",
            "Understood.",
        )?;
        let job_id = db.enqueue_job(
            EXTRACT_MEMORY_JOB,
            Some(CONVERSATION_TARGET),
            Some(&turn_id.to_string()),
            0,
            Some("2026-09-12T10:00:00Z"),
        )?;

        let (embedding_url, handle) = start_embedding_server()?;
        let indexer = orchestrator::PersistentMemoryIndexer::new(
            &database,
            &memory_root,
            embedding_url,
            "test-embedding-model",
        )?;
        let model = FakeModel {
            output: r#"{"decision":"should_save","items":[{"kind":"fact","title":"User name","body":"Pranav prefers to be called Optimus Prime.","tags":["identity"],"entities":[],"relationships":[],"events":[],"states":[],"task_status":null,"reminder_status":null,"due_at":null}]}"#.to_string(),
            calls: Mutex::new(0),
        };
        let worker = MemoryExtractionWorker::new(db, model).with_auto_apply_indexer(indexer);

        worker.run_once()?;

        let job = worker.db.job(&job_id)?.ok_or("job missing")?;
        assert_eq!(job.status, sqlite_memory::JobStatus::Completed);
        let proposal = worker
            .db
            .latest_memory_extraction_proposal_for_turn(turn_id)?
            .ok_or("proposal missing")?;
        assert_eq!(proposal.decision, "should_save");
        assert_eq!(proposal.status, "applied");

        let extracted_dir = memory_root.join("journal/extracted");
        let extracted_entries =
            std::fs::read_dir(&extracted_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(extracted_entries.len(), 1);

        handle
            .join()
            .map_err(|_| "embedding mock server thread panicked")?;
        Ok(())
    }

    #[test]
    fn explicit_must_save_is_applied_automatically() -> Result<()> {
        let (_directory, db) = test_db()?;
        let database = _directory.path().join("worker.db");
        let memory_root = _directory.path().join("memory");
        std::fs::create_dir_all(&memory_root)?;

        let turn_id = db.append_conversation_turn(
            "session-1",
            "My name is Pranav. Please save that.",
            "I saved it to persistent memory.",
        )?;
        let job_id = db.enqueue_job(
            EXTRACT_MEMORY_JOB,
            Some(CONVERSATION_TARGET),
            Some(&turn_id.to_string()),
            0,
            Some("2026-09-12T10:00:00Z"),
        )?;

        let (embedding_url, handle) = start_embedding_server()?;
        let indexer = orchestrator::PersistentMemoryIndexer::new(
            &database,
            &memory_root,
            embedding_url,
            "test-embedding-model",
        )?;
        let model = FakeModel {
            output: r#"{"decision":"must_save","items":[{"kind":"fact","title":"User name","body":"Pranav is the user's name.","tags":[],"entities":[],"relationships":[],"events":[],"states":[],"task_status":null,"reminder_status":null,"due_at":null}]}"#.to_string(),
            calls: Mutex::new(0),
        };
        let worker = MemoryExtractionWorker::new(db, model).with_auto_apply_indexer(indexer);

        worker.run_once()?;

        let job = worker.db.job(&job_id)?.ok_or("job missing")?;
        assert_eq!(job.status, sqlite_memory::JobStatus::Completed);
        let proposal = worker
            .db
            .latest_memory_extraction_proposal_for_turn(turn_id)?
            .ok_or("proposal missing")?;
        assert_eq!(proposal.decision, "must_save");
        assert_eq!(proposal.status, "applied");

        let extracted_dir = memory_root.join("journal/extracted");
        let extracted_entries =
            std::fs::read_dir(&extracted_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(extracted_entries.len(), 1);
        assert!(
            extracted_entries[0]
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("md")
        );

        handle
            .join()
            .map_err(|_| "embedding mock server thread panicked")?;
        Ok(())
    }

    #[test]
    fn malformed_extraction_job_is_failed_for_retry() -> Result<()> {
        let (_directory, db) = test_db()?;
        let job_id = db.enqueue_job(
            EXTRACT_MEMORY_JOB,
            Some(CONVERSATION_TARGET),
            Some("not-an-integer"),
            0,
            Some("2026-09-12T10:00:00Z"),
        )?;
        let model = FakeModel {
            output: r#"{"decision":"dont_save","items":[]}"#.to_string(),
            calls: Mutex::new(0),
        };
        let worker = MemoryExtractionWorker::new(db, model);
        assert!(worker.run_once().is_err());
        let job = worker.db.job(&job_id)?.ok_or("job missing")?;
        assert_eq!(job.status, sqlite_memory::JobStatus::Failed);
        assert_eq!(job.attempts, 1);
        assert!(
            job.last_error
                .as_deref()
                .unwrap_or_default()
                .contains("invalid digit")
        );
        Ok(())
    }

    #[test]
    fn worker_reuses_existing_proposal_without_recalling_model() -> Result<()> {
        let (_directory, db) = test_db()?;
        let turn_id = db.append_conversation_turn("session-1", "Hello", "Hi")?;
        db.store_memory_extraction_proposal(
            turn_id,
            "dont_save",
            r#"{"decision":"dont_save","items":[]}"#,
        )?;
        db.enqueue_job(
            EXTRACT_MEMORY_JOB,
            Some(CONVERSATION_TARGET),
            Some(&turn_id.to_string()),
            0,
            Some("2026-09-12T10:00:00Z"),
        )?;
        let model = FakeModel {
            output: r#"{"decision":"dont_save","items":[]}"#.to_string(),
            calls: Mutex::new(0),
        };
        let worker = MemoryExtractionWorker::new(db, model);
        worker.run_once()?;
        assert_eq!(
            *worker
                .models
                .calls
                .lock()
                .map_err(|_| "calls mutex poisoned.")?,
            0
        );
        Ok(())
    }
}
