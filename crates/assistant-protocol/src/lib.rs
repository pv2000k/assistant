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
    },
    ModelList,
    ModelStatus,
    ModelSwitch {
        model: String,
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
    Models(ModelList),
    ModelStatus(ModelState),
    Tasks(TaskList),
    Reminders(ReminderList),
    Memory(MemorySearchResult),
    Proposals(MemoryProposalList),
    Jobs(JobList),
    Mutation(MutationResult),
    Lease(LeaseStatus),
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
    pub status: String,
    pub due_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderList {
    pub reminders: Vec<ReminderSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    pub due_at: Option<String>,
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
    fn default_limits_are_applied() {
        assert_eq!(RequestMethod::list_limit(None), DEFAULT_LIST_LIMIT);
        assert_eq!(RequestMethod::list_limit(Some(0)), 1);
        assert_eq!(RequestMethod::list_limit(Some(25)), 25);
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
