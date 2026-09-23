use serde::{Deserialize, Deserializer, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_LIST_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRequest {
    pub id: u64,
    pub version: u32,
    pub method: RequestMethod,
}

impl WireRequest {
    pub fn new(id: u64, method: RequestMethod) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            method,
        }
    }

    pub fn encode_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }

    pub fn decode_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line.trim_end_matches(['\r', '\n']))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum RequestMethod {
    Ping,
    Health,
    Chat {
        text: String,
        #[serde(default)]
        attachments: Vec<ChatAttachment>,
    },
    SessionNew,
    SessionHistory {
        #[serde(default)]
        limit: Option<usize>,
    },
    ModelList,
    ModelStatus,
    ModelSwitch {
        model: String,
    },
    SystemInfo {
        #[serde(default)]
        scope: Option<String>,
    },
    TasksList {
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    RemindersList {
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    MemorySearch {
        query: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    MemoryProposals {
        #[serde(default)]
        limit: Option<usize>,
    },
    MemoryProposalAccept {
        id: String,
    },
    MemoryProposalReject {
        id: String,
    },
    JobsList {
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    CalendarLogin,
    CalendarStatus,
    CalendarEventsList {
        #[serde(default)]
        calendar_id: Option<String>,
        #[serde(default)]
        time_min: Option<String>,
        #[serde(default)]
        time_max: Option<String>,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        max_results: Option<u64>,
        #[serde(default)]
        page_token: Option<String>,
    },
    CalendarEventGet {
        #[serde(default)]
        calendar_id: Option<String>,
        event_id: String,
    },
    CalendarEventMutate {
        operation: String,
        arguments: serde_json::Value,
    },
    TasksMutate {
        operation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_optional_string"
        )]
        due: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    RemindersMutate {
        operation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_optional_string"
        )]
        due: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        calendar_sync: Option<bool>,
    },
    ApprovalsList,
    ApprovalRespond {
        id: u64,
        approved: bool,
    },
    ClientAcquire {
        client_id: String,
    },
    ClientHeartbeat {
        client_id: String,
    },
    ClientRelease {
        client_id: String,
    },
}

fn deserialize_optional_optional_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

impl RequestMethod {
    pub fn list_limit(limit: Option<usize>) -> usize {
        limit.unwrap_or(DEFAULT_LIST_LIMIT).max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireResponse {
    pub id: u64,
    pub version: u32,
    pub result: Option<ResponsePayload>,
    pub error: Option<WireError>,
}

impl WireResponse {
    pub fn ok(id: u64, result: ResponsePayload) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            result: None,
            error: Some(WireError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    pub fn encode_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }

    pub fn decode_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line.trim_end_matches(['\r', '\n']))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ResponsePayload {
    Pong,
    Health(HealthStatus),
    Chat(ChatResponse),
    Session(SessionState),
    SessionHistory(SessionHistoryResponse),
    Models(ModelList),
    ModelStatus(ModelState),
    SystemInfo(serde_json::Value),
    Tasks(TaskList),
    Reminders(ReminderList),
    Memory(MemorySearchResult),
    Proposals(MemoryProposalList),
    Jobs(JobList),
    CalendarStatus(CalendarStatus),
    CalendarEvents(CalendarEvents),
    CalendarEvent(CalendarEvent),
    Mutation(MutationResult),
    Approvals(ApprovalList),
    Approval(ApprovalStatus),
    Lease(LeaseStatus),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatAttachment {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    pub runtime: String,
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub turn_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatTurn {
    pub user: String,
    pub assistant: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryResponse {
    pub session_id: String,
    pub turns: Vec<ChatTurn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequestSummary {
    pub id: u64,
    pub tool: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalList {
    pub approvals: Vec<ApprovalRequestSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalStatus {
    pub id: u64,
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelList {
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub available: bool,
    pub active: bool,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelState {
    pub active_model: Option<String>,
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskList {
    pub tasks: Vec<TaskSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub due_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderList {
    pub reminders: Vec<ReminderSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderSummary {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub due_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub calendar_sync_enabled: bool,
    pub calendar_sync_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub results: Vec<MemorySummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySummary {
    pub id: String,
    pub text: String,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryProposalList {
    pub proposals: Vec<MemoryProposalSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryProposalSummary {
    pub id: String,
    pub decision: String,
    pub conversation_turn_id: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: String,
    pub proposed_memory: String,
    pub item_kind: String,
    pub tags: Vec<String>,
    pub item_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobList {
    pub jobs: Vec<JobSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: String,
    pub job_type: String,
    pub status: String,
    pub next_run_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarStatus {
    pub configured: bool,
    pub authenticated: bool,
    pub write_enabled: bool,
    pub calendar_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarEvents {
    pub calendar_id: String,
    pub events: Vec<serde_json::Value>,
    pub count: usize,
    pub next_page_token: Option<String>,
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub calendar_id: String,
    pub event: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationResult {
    pub tool: String,
    pub operation: String,
    pub output: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseStatus {
    pub client_id: String,
    pub active_clients: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_new_request_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(6, RequestMethod::SessionNew);
        let encoded = request.encode_line()?;
        let decoded = WireRequest::decode_line(&encoded)?;
        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn session_history_request_and_response_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(7, RequestMethod::SessionHistory { limit: Some(12) });
        let encoded = request.encode_line()?;
        let decoded = WireRequest::decode_line(&encoded)?;
        assert_eq!(decoded, request);

        let response = WireResponse::ok(
            7,
            ResponsePayload::SessionHistory(SessionHistoryResponse {
                session_id: "session-test".to_string(),
                turns: vec![ChatTurn {
                    user: "Hello".to_string(),
                    assistant: "Hi there.".to_string(),
                }],
            }),
        );
        let response_encoded = response.encode_line()?;
        let response_decoded = WireResponse::decode_line(&response_encoded)?;
        assert_eq!(response_decoded, response);
        Ok(())
    }

    #[test]
    fn model_switch_request_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(
            7,
            RequestMethod::ModelSwitch {
                model: "gemma-4-e4b".to_string(),
            },
        );

        let encoded = request.encode_line()?;
        let decoded = WireRequest::decode_line(&encoded)?;

        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn chat_request_with_attachment_round_trips() {
        let request = WireRequest::new(
            8,
            RequestMethod::Chat {
                text: "Summarize this file".to_string(),
                attachments: vec![ChatAttachment {
                    path: "/tmp/report.txt".to_string(),
                }],
            },
        );

        let encoded = request.encode_line().expect("encode request");
        let decoded = WireRequest::decode_line(&encoded).expect("decode request");
        assert_eq!(decoded, request);
    }

    #[test]
    fn default_limits_are_applied() {
        assert_eq!(RequestMethod::list_limit(None), DEFAULT_LIST_LIMIT);
        assert_eq!(RequestMethod::list_limit(Some(0)), 1);
        assert_eq!(RequestMethod::list_limit(Some(25)), 25);
    }

    #[test]
    fn system_info_request_and_response_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(
            8,
            RequestMethod::SystemInfo {
                scope: Some("all".to_string()),
            },
        );
        let encoded = request.encode_line()?;
        let decoded = WireRequest::decode_line(&encoded)?;
        assert_eq!(decoded, request);

        let response = WireResponse::ok(
            8,
            ResponsePayload::SystemInfo(serde_json::json!({
                "runtime": {"ready": true}
            })),
        );
        let encoded = response.encode_line()?;
        let decoded = WireResponse::decode_line(&encoded)?;
        assert_eq!(decoded, response);
        Ok(())
    }

    #[test]
    fn error_response_has_stable_wire_shape() -> Result<(), Box<dyn std::error::Error>> {
        let response =
            WireResponse::error(42, "model_unavailable", "Requested model is unavailable.");

        let encoded = response.encode_line()?;
        let decoded = WireResponse::decode_line(&encoded)?;

        assert_eq!(decoded.id, 42);
        assert_eq!(
            decoded.error,
            Some(WireError {
                code: "model_unavailable".to_string(),
                message: "Requested model is unavailable.".to_string(),
            })
        );
        assert!(decoded.result.is_none());
        Ok(())
    }

    #[test]
    fn task_mutation_omits_unset_optional_fields() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(
            13,
            RequestMethod::TasksMutate {
                operation: "complete".to_string(),
                id: Some("task:rcm-report".to_string()),
                title: None,
                body: None,
                due: None,
                status: None,
            },
        );

        let value: serde_json::Value = serde_json::from_str(&request.encode_line()?)?;
        assert_eq!(
            value["method"]["method"],
            serde_json::Value::String("tasks_mutate".to_string())
        );
        assert_eq!(
            value["method"]["params"],
            serde_json::json!({
                "operation": "complete",
                "id": "task:rcm-report"
            })
        );
        Ok(())
    }

    #[test]
    fn task_mutation_preserves_explicit_null_due() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(
            14,
            RequestMethod::TasksMutate {
                operation: "update".to_string(),
                id: Some("task:rcm-report".to_string()),
                title: None,
                body: None,
                due: Some(None),
                status: None,
            },
        );

        let value: serde_json::Value = serde_json::from_str(&request.encode_line()?)?;
        assert_eq!(
            value["method"]["params"],
            serde_json::json!({
                "operation": "update",
                "id": "task:rcm-report",
                "due": null
            })
        );

        let decoded = WireRequest::decode_line(&request.encode_line()?)?;
        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn reminder_mutation_omits_unset_optional_fields() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(
            15,
            RequestMethod::RemindersMutate {
                operation: "cancel".to_string(),
                id: Some("reminder:review-rcm".to_string()),
                title: None,
                body: None,
                due: None,
                calendar_sync: None,
            },
        );

        let value: serde_json::Value = serde_json::from_str(&request.encode_line()?)?;
        assert_eq!(
            value["method"]["params"],
            serde_json::json!({
                "operation": "cancel",
                "id": "reminder:review-rcm"
            })
        );
        Ok(())
    }

    #[test]
    fn reminder_mutation_preserves_explicit_null_due() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(
            16,
            RequestMethod::RemindersMutate {
                operation: "update".to_string(),
                id: Some("reminder:review-rcm".to_string()),
                title: None,
                body: None,
                due: Some(None),
                calendar_sync: None,
            },
        );

        let value: serde_json::Value = serde_json::from_str(&request.encode_line()?)?;
        assert_eq!(
            value["method"]["params"],
            serde_json::json!({
                "operation": "update",
                "id": "reminder:review-rcm",
                "due": null
            })
        );

        let decoded = WireRequest::decode_line(&request.encode_line()?)?;
        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn reminder_mutation_request_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(
            17,
            RequestMethod::RemindersMutate {
                operation: "update".to_string(),
                id: Some("reminder:review-rcm".to_string()),
                title: Some("Review RCM report".to_string()),
                body: Some("Send notes to finance.".to_string()),
                due: Some(Some("tomorrow at 6 PM".to_string())),
                calendar_sync: Some(true),
            },
        );

        let encoded = request.encode_line()?;
        let decoded = WireRequest::decode_line(&encoded)?;

        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn memory_proposal_mutation_requests_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let accept = WireRequest::new(
            18,
            RequestMethod::MemoryProposalAccept {
                id: "proposal-123".to_string(),
            },
        );
        let reject = WireRequest::new(
            19,
            RequestMethod::MemoryProposalReject {
                id: "proposal-456".to_string(),
            },
        );

        assert_eq!(WireRequest::decode_line(&accept.encode_line()?)?, accept);
        assert_eq!(WireRequest::decode_line(&reject.encode_line()?)?, reject);
        Ok(())
    }

    #[test]
    fn memory_proposal_summary_response_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let response = WireResponse::ok(
            21,
            ResponsePayload::Proposals(MemoryProposalList {
                proposals: vec![MemoryProposalSummary {
                    id: "proposal-1".to_string(),
                    decision: "should_save".to_string(),
                    conversation_turn_id: "42".to_string(),
                    status: "pending".to_string(),
                    created_at: "2026-09-21T12:00:00Z".to_string(),
                    updated_at: "2026-09-21T12:00:00Z".to_string(),
                    title: "Test memory".to_string(),
                    proposed_memory: "A durable fact.".to_string(),
                    item_kind: "fact".to_string(),
                    tags: vec!["project:zaraki".to_string()],
                    item_count: 1,
                }],
            }),
        );

        let decoded = WireResponse::decode_line(&response.encode_line()?)?;
        assert_eq!(decoded, response);
        Ok(())
    }

    #[test]
    fn calendar_requests_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let list = WireRequest::new(
            20,
            RequestMethod::CalendarEventsList {
                calendar_id: Some("primary".to_string()),
                time_min: Some("2026-09-22T00:00:00Z".to_string()),
                time_max: None,
                query: Some("Zaraki".to_string()),
                max_results: Some(10),
                page_token: None,
            },
        );
        let get = WireRequest::new(
            21,
            RequestMethod::CalendarEventGet {
                calendar_id: None,
                event_id: "event-123".to_string(),
            },
        );
        let mutate = WireRequest::new(
            22,
            RequestMethod::CalendarEventMutate {
                operation: "create".to_string(),
                arguments: serde_json::json!({
                    "summary": "Zaraki reminder",
                    "start": {"dateTime": "2026-09-22T18:00:00+05:30"},
                    "end": {"dateTime": "2026-09-22T18:30:00+05:30"}
                }),
            },
        );

        assert_eq!(WireRequest::decode_line(&list.encode_line()?)?, list);
        assert_eq!(WireRequest::decode_line(&get.encode_line()?)?, get);
        assert_eq!(WireRequest::decode_line(&mutate.encode_line()?)?, mutate);
        Ok(())
    }

    #[test]
    fn calendar_login_request_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(19, RequestMethod::CalendarLogin);
        assert_eq!(WireRequest::decode_line(&request.encode_line()?)?, request);
        Ok(())
    }

    #[test]
    fn calendar_status_response_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let response = WireResponse::ok(
            23,
            ResponsePayload::CalendarStatus(CalendarStatus {
                configured: true,
                authenticated: true,
                write_enabled: true,
                calendar_id: "primary".to_string(),
            }),
        );

        assert_eq!(
            WireResponse::decode_line(&response.encode_line()?)?,
            response
        );
        Ok(())
    }

    #[test]
    fn approval_requests_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let list = WireRequest::new(24, RequestMethod::ApprovalsList);
        let respond = WireRequest::new(
            25,
            RequestMethod::ApprovalRespond {
                id: 7,
                approved: true,
            },
        );

        assert_eq!(WireRequest::decode_line(&list.encode_line()?)?, list);
        assert_eq!(WireRequest::decode_line(&respond.encode_line()?)?, respond);
        Ok(())
    }

    #[test]
    fn approval_responses_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let response = WireResponse::ok(
            26,
            ResponsePayload::Approvals(ApprovalList {
                approvals: vec![ApprovalRequestSummary {
                    id: 7,
                    tool: "reminders.mutate".to_string(),
                    arguments: serde_json::json!({
                        "operation": "create",
                        "title": "Review RCM report"
                    }),
                }],
            }),
        );

        assert_eq!(
            WireResponse::decode_line(&response.encode_line()?)?,
            response
        );

        let decision = WireResponse::ok(
            27,
            ResponsePayload::Approval(ApprovalStatus {
                id: 7,
                approved: true,
            }),
        );
        assert_eq!(
            WireResponse::decode_line(&decision.encode_line()?)?,
            decision
        );

        Ok(())
    }

    #[test]
    fn client_lease_requests_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let requests = [
            RequestMethod::ClientAcquire {
                client_id: "tui-1".to_string(),
            },
            RequestMethod::ClientHeartbeat {
                client_id: "tui-1".to_string(),
            },
            RequestMethod::ClientRelease {
                client_id: "tui-1".to_string(),
            },
        ];

        for request in requests {
            let wire = WireRequest::new(99, request.clone());
            assert_eq!(WireRequest::decode_line(&wire.encode_line()?)?, wire);
        }

        let response = WireResponse::ok(
            100,
            ResponsePayload::Lease(LeaseStatus {
                client_id: "tui-1".to_string(),
                active_clients: 1,
            }),
        );
        assert_eq!(
            WireResponse::decode_line(&response.encode_line()?)?,
            response
        );
        Ok(())
    }

    #[test]
    fn task_mutation_request_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let request = WireRequest::new(
            12,
            RequestMethod::TasksMutate {
                operation: "update".to_string(),
                id: Some("task:rcm-report".to_string()),
                title: Some("Review RCM report".to_string()),
                body: None,
                due: Some(Some("tomorrow at 6 PM".to_string())),
                status: Some("in_progress".to_string()),
            },
        );

        let encoded = request.encode_line()?;
        let decoded = WireRequest::decode_line(&encoded)?;

        assert_eq!(decoded, request);
        Ok(())
    }
}
