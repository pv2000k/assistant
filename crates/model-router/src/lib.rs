use orchestrator::{ModelCall, ModelExecutor, ModelResult, ModelTarget};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    error::Error,
    sync::{Arc, RwLock},
    time::Duration,
};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Controller,
    GeneralResponse,
    CodingAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelBackendInfo {
    pub target: ModelTarget,
    pub capabilities: Vec<ModelCapability>,
}

pub trait ModelBackend: Send + Sync {
    fn target(&self) -> ModelTarget;
    fn capabilities(&self) -> &[ModelCapability];
    fn execute(&self, prompt: &str) -> Result<String>;

    fn execute_with_response_format(
        &self,
        prompt: &str,
        response_format: Option<&serde_json::Value>,
    ) -> Result<String> {
        let _ = response_format;
        self.execute(prompt)
    }

    fn set_model(&self, _model: &str) -> Result<()> {
        Err("This model backend does not support runtime model switching.".into())
    }
}

#[derive(Debug, Clone)]
struct QwenBackend {
    base_url: String,
    model: Arc<RwLock<String>>,
    timeout: Duration,
    capabilities: Vec<ModelCapability>,
}

impl QwenBackend {
    #[cfg(test)]
    fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_shared_model(base_url, Arc::new(RwLock::new(model.into())))
    }

    fn with_shared_model(base_url: impl Into<String>, model: Arc<RwLock<String>>) -> Self {
        Self {
            base_url: base_url.into(),
            model,
            timeout: Duration::from_secs(120),
            capabilities: vec![
                ModelCapability::Controller,
                ModelCapability::GeneralResponse,
            ],
        }
    }

    fn current_model(&self) -> Result<String> {
        Ok(self
            .model
            .read()
            .map_err(|_| "Qwen model state lock was poisoned.")?
            .clone())
    }

    fn set_model(&self, model: &str) -> Result<()> {
        let model = model.trim();
        if model.is_empty() {
            return Err("Qwen model cannot be empty.".into());
        }

        *self
            .model
            .write()
            .map_err(|_| "Qwen model state lock was poisoned.")? = model.to_string();
        Ok(())
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }
}

#[derive(Debug, Clone)]
struct ClaudeBackend {
    api_key: String,
    base_url: String,
    model: String,
    timeout: Duration,
    capabilities: Vec<ModelCapability>,
}

impl ClaudeBackend {
    fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://api.anthropic.com")
    }

    fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            timeout: Duration::from_secs(120),
            capabilities: vec![ModelCapability::GeneralResponse],
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }
}

#[derive(Debug, Clone)]
struct CodexBackend {
    api_key: String,
    base_url: String,
    model: String,
    timeout: Duration,
    capabilities: Vec<ModelCapability>,
}

impl CodexBackend {
    fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://api.openai.com")
    }

    fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            timeout: Duration::from_secs(120),
            capabilities: vec![
                ModelCapability::GeneralResponse,
                ModelCapability::CodingAgent,
            ],
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/responses", self.base_url.trim_end_matches('/'))
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeMessageResponse {
    content: Vec<ClaudeContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ClaudeContentBlock {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    output: Vec<OpenAiOutputItem>,
    #[serde(default)]
    output_text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiOutputItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Vec<OpenAiContentItem>,
}

#[derive(Debug, Deserialize)]
struct OpenAiContentItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

fn validate_api_key(value: &str, provider: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(format!("{provider} API key cannot be empty.").into());
    }

    Ok(())
}

impl ModelBackend for ClaudeBackend {
    fn target(&self) -> ModelTarget {
        ModelTarget::Claude
    }

    fn capabilities(&self) -> &[ModelCapability] {
        &self.capabilities
    }

    fn execute(&self, prompt: &str) -> Result<String> {
        if prompt.trim().is_empty() {
            return Err("Model prompt cannot be empty.".into());
        }

        validate_api_key(&self.api_key, "Claude")?;

        let client = Client::builder().timeout(self.timeout).build()?;

        let response = client
            .post(self.endpoint())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": self.model,
                "max_tokens": 1024,
                "messages": [
                    {
                        "role": "user",
                        "content": prompt
                    }
                ],
                "stream": false
            }))
            .send()?;

        let status = response.status();
        let body = response.text()?;

        if !status.is_success() {
            return Err(format!("Claude returned HTTP {}: {}", status, body).into());
        }

        let parsed: ClaudeMessageResponse = serde_json::from_str(&body)?;
        let content = parsed
            .content
            .into_iter()
            .filter(|block| block.kind == "text")
            .filter_map(|block| block.text)
            .collect::<Vec<_>>()
            .join("");

        if content.trim().is_empty() {
            return Err("Claude returned no text content.".into());
        }

        Ok(content)
    }
}

impl ModelBackend for CodexBackend {
    fn target(&self) -> ModelTarget {
        ModelTarget::Codex
    }

    fn capabilities(&self) -> &[ModelCapability] {
        &self.capabilities
    }

    fn execute(&self, prompt: &str) -> Result<String> {
        if prompt.trim().is_empty() {
            return Err("Model prompt cannot be empty.".into());
        }

        validate_api_key(&self.api_key, "OpenAI")?;

        let client = Client::builder().timeout(self.timeout).build()?;

        let response = client
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({
                "model": self.model,
                "input": prompt,
                "max_output_tokens": 1024,
                "stream": false
            }))
            .send()?;

        let status = response.status();
        let body = response.text()?;

        if !status.is_success() {
            return Err(format!("Codex returned HTTP {}: {}", status, body).into());
        }

        let parsed: OpenAiResponse = serde_json::from_str(&body)?;

        if let Some(output_text) = parsed.output_text {
            if !output_text.trim().is_empty() {
                return Ok(output_text);
            }
        }

        let content = parsed
            .output
            .into_iter()
            .filter(|item| item.kind == "message")
            .flat_map(|item| item.content)
            .filter(|item| item.kind == "output_text")
            .filter_map(|item| item.text)
            .collect::<Vec<_>>()
            .join("");

        if content.trim().is_empty() {
            return Err("Codex returned no text content.".into());
        }

        Ok(content)
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: String,
}

impl ModelBackend for QwenBackend {
    fn target(&self) -> ModelTarget {
        ModelTarget::Qwen
    }

    fn capabilities(&self) -> &[ModelCapability] {
        &self.capabilities
    }

    fn set_model(&self, model: &str) -> Result<()> {
        self.set_model(model)
    }

    fn execute(&self, prompt: &str) -> Result<String> {
        self.execute_with_response_format(prompt, None)
    }

    fn execute_with_response_format(
        &self,
        prompt: &str,
        response_format: Option<&serde_json::Value>,
    ) -> Result<String> {
        if prompt.trim().is_empty() {
            return Err("Model prompt cannot be empty.".into());
        }

        let client = Client::builder().timeout(self.timeout).build()?;

        let mut request = serde_json::json!({
            "model": self.current_model()?,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "temperature": 0,
            "max_tokens": 1024,
            "stream": false,
            "reasoning_effort": "none"
        });

        if let Some(schema) = response_format {
            request["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "structured_response",
                    "strict": true,
                    "schema": schema
                }
            });
        }

        let response = client.post(self.endpoint()).json(&request).send()?;

        let status = response.status();
        let body = response.text()?;

        if !status.is_success() {
            return Err(format!("Qwen returned HTTP {}: {}", status, body).into());
        }

        let parsed: ChatCompletionResponse = serde_json::from_str(&body)?;

        let content = parsed
            .choices
            .first()
            .ok_or("Qwen returned no choices.")?
            .message
            .content
            .clone();

        if content.trim().is_empty() {
            return Err("Qwen returned an empty response.".into());
        }

        Ok(content)
    }
}

pub struct ModelRouter {
    backends: HashMap<ModelTarget, Box<dyn ModelBackend>>,
}

impl std::fmt::Debug for ModelRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelRouter")
            .field("backends", &self.available_targets())
            .finish()
    }
}

impl ModelRouter {
    pub fn new(qwen_url: impl Into<String>, qwen_model: impl Into<String>) -> Self {
        Self::new_with_shared_qwen_model(qwen_url, Arc::new(RwLock::new(qwen_model.into())))
    }

    pub fn new_with_shared_qwen_model(
        qwen_url: impl Into<String>,
        qwen_model: Arc<RwLock<String>>,
    ) -> Self {
        let mut router = Self::empty();
        router
            .register(QwenBackend::with_shared_model(qwen_url, qwen_model))
            .expect("Qwen backend registration must not fail in a fresh router.");
        router
    }

    pub fn set_qwen_model(&self, model: &str) -> Result<()> {
        let backend = self
            .backends
            .get(&ModelTarget::Qwen)
            .ok_or("Qwen backend is not registered.")?;
        backend.set_model(model)
    }

    pub fn empty() -> Self {
        Self {
            backends: HashMap::new(),
        }
    }

    pub fn register<B>(&mut self, backend: B) -> Result<()>
    where
        B: ModelBackend + 'static,
    {
        let target = backend.target();

        if self.backends.contains_key(&target) {
            return Err(format!("A backend for {:?} is already registered.", target).into());
        }

        self.backends.insert(target, Box::new(backend));
        Ok(())
    }

    pub fn available_targets(&self) -> Vec<ModelTarget> {
        let mut targets = self.backends.keys().cloned().collect::<Vec<_>>();
        targets.sort_by_key(|target| format!("{target:?}"));
        targets
    }

    pub fn backend_info(&self, target: &ModelTarget) -> Option<ModelBackendInfo> {
        self.backends.get(target).map(|backend| ModelBackendInfo {
            target: target.clone(),
            capabilities: backend.capabilities().to_vec(),
        })
    }

    pub fn supports(&self, target: &ModelTarget, capability: ModelCapability) -> bool {
        self.backends
            .get(target)
            .is_some_and(|backend| backend.capabilities().contains(&capability))
    }

    pub fn register_claude(
        &mut self,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<()> {
        self.register(ClaudeBackend::new(api_key, model))
    }

    pub fn register_claude_with_base_url(
        &mut self,
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<()> {
        self.register(ClaudeBackend::with_base_url(api_key, model, base_url))
    }

    pub fn register_codex(
        &mut self,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<()> {
        self.register(CodexBackend::new(api_key, model))
    }

    pub fn register_codex_with_base_url(
        &mut self,
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<()> {
        self.register(CodexBackend::with_base_url(api_key, model, base_url))
    }

    fn execute_target(&self, call: &ModelCall) -> Result<String> {
        self.execute_target_with_response_format(call, None)
    }

    fn execute_target_with_response_format(
        &self,
        call: &ModelCall,
        response_format: Option<&serde_json::Value>,
    ) -> Result<String> {
        let backend = self.backends.get(&call.target).ok_or_else(|| {
            format!(
                "No backend is registered for model target {:?}.",
                call.target
            )
        })?;

        backend.execute_with_response_format(&call.prompt, response_format)
    }
}

impl ModelExecutor for ModelRouter {
    fn execute(&self, call: &ModelCall) -> Result<ModelResult> {
        if call.prompt.trim().is_empty() {
            return Err("Model prompt cannot be empty.".into());
        }

        let output = self.execute_target(call)?;

        Ok(ModelResult {
            target: call.target.clone(),
            output,
        })
    }

    fn execute_with_response_format(
        &self,
        call: &ModelCall,
        response_format: Option<&serde_json::Value>,
    ) -> Result<ModelResult> {
        if call.prompt.trim().is_empty() {
            return Err("Model prompt cannot be empty.".into());
        }

        let output = self.execute_target_with_response_format(call, response_format)?;

        Ok(ModelResult {
            target: call.target.clone(),
            output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeBackend {
        target: ModelTarget,
        capabilities: Vec<ModelCapability>,
        response: String,
    }

    impl ModelBackend for FakeBackend {
        fn target(&self) -> ModelTarget {
            self.target.clone()
        }

        fn capabilities(&self) -> &[ModelCapability] {
            &self.capabilities
        }

        fn execute(&self, prompt: &str) -> Result<String> {
            Ok(format!("{}: {}", self.response, prompt))
        }
    }

    #[test]
    fn empty_prompt_is_rejected() {
        let router = ModelRouter::new("http://127.0.0.1:8080", "Qwen3.5-4B-Q4_K_M.gguf");

        let result = router.execute(&ModelCall {
            target: ModelTarget::Qwen,
            prompt: "   ".to_string(),
        });

        assert!(result.is_err());
    }

    #[test]
    fn qwen_is_registered_with_expected_capabilities() {
        let router = ModelRouter::new("http://127.0.0.1:8080", "Qwen3.5-4B-Q4_K_M.gguf");

        assert_eq!(router.available_targets(), vec![ModelTarget::Qwen]);
        assert!(router.supports(&ModelTarget::Qwen, ModelCapability::Controller));
        assert!(router.supports(&ModelTarget::Qwen, ModelCapability::GeneralResponse));
        assert!(!router.supports(&ModelTarget::Qwen, ModelCapability::CodingAgent));
    }

    #[test]
    fn qwen_model_can_be_switched_through_shared_state() -> Result<()> {
        let shared = Arc::new(RwLock::new("old.gguf".to_string()));
        let router =
            ModelRouter::new_with_shared_qwen_model("http://127.0.0.1:8080", Arc::clone(&shared));

        router.set_qwen_model("new.gguf")?;

        assert_eq!(
            shared
                .read()
                .map_err(|_| "shared model lock was poisoned.")?
                .as_str(),
            "new.gguf"
        );
        Ok(())
    }

    #[test]
    fn unconfigured_claude_is_rejected() {
        let router = ModelRouter::new("http://127.0.0.1:8080", "Qwen3.5-4B-Q4_K_M.gguf");

        let result = router.execute(&ModelCall {
            target: ModelTarget::Claude,
            prompt: "hello".to_string(),
        });

        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("Claude"));
    }

    #[test]
    fn unconfigured_codex_is_rejected() {
        let router = ModelRouter::new("http://127.0.0.1:8080", "Qwen3.5-4B-Q4_K_M.gguf");

        let result = router.execute(&ModelCall {
            target: ModelTarget::Codex,
            prompt: "hello".to_string(),
        });

        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("Codex"));
    }

    #[test]
    fn custom_backend_can_be_registered_and_routed() -> Result<()> {
        let mut router = ModelRouter::empty();

        router.register(FakeBackend {
            target: ModelTarget::Claude,
            capabilities: vec![ModelCapability::GeneralResponse],
            response: "fake-claude".to_string(),
        })?;

        let result = router.execute(&ModelCall {
            target: ModelTarget::Claude,
            prompt: "hello".to_string(),
        })?;

        assert_eq!(result.target, ModelTarget::Claude);
        assert_eq!(result.output, "fake-claude: hello");
        assert!(router.supports(&ModelTarget::Claude, ModelCapability::GeneralResponse));

        Ok(())
    }

    #[test]
    fn claude_backend_is_registered_with_general_response_capability() -> Result<()> {
        let mut router = ModelRouter::empty();
        router.register_claude("test-key", "claude-sonnet-5")?;
        assert_eq!(router.available_targets(), vec![ModelTarget::Claude]);
        assert!(router.supports(&ModelTarget::Claude, ModelCapability::GeneralResponse));
        assert!(!router.supports(&ModelTarget::Claude, ModelCapability::Controller));
        Ok(())
    }

    #[test]
    fn codex_backend_is_registered_with_coding_capability() -> Result<()> {
        let mut router = ModelRouter::empty();
        router.register_codex("test-key", "gpt-5.3-codex")?;
        assert_eq!(router.available_targets(), vec![ModelTarget::Codex]);
        assert!(router.supports(&ModelTarget::Codex, ModelCapability::CodingAgent));
        assert!(router.supports(&ModelTarget::Codex, ModelCapability::GeneralResponse));
        Ok(())
    }

    #[test]
    fn claude_backend_sends_expected_request_and_parses_text() -> Result<()> {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server accept failed");
            let mut buffer = [0u8; 4096];
            let size = stream.read(&mut buffer).expect("mock server read failed");
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request.starts_with("POST /v1/messages"));
            assert!(request.to_ascii_lowercase().contains("x-api-key: test-key"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("anthropic-version: 2023-06-01")
            );
            assert!(request.contains("claude-sonnet-5"));
            assert!(request.contains("Hello Claude"));
            let body = r#"{"content":[{"type":"text","text":"Hello from Claude."}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("mock server write failed");
        });
        let mut router = ModelRouter::empty();
        router.register_claude_with_base_url(
            "test-key",
            "claude-sonnet-5",
            format!("http://{address}"),
        )?;
        let result = router.execute(&ModelCall {
            target: ModelTarget::Claude,
            prompt: "Hello Claude".to_string(),
        })?;
        assert_eq!(result.output, "Hello from Claude.");
        handle.join().map_err(|_| "mock server thread panicked")?;
        Ok(())
    }

    #[test]
    fn codex_backend_sends_expected_request_and_parses_output() -> Result<()> {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server accept failed");
            let mut buffer = [0u8; 4096];
            let size = stream.read(&mut buffer).expect("mock server read failed");
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request.starts_with("POST /v1/responses"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-key")
            );
            assert!(request.contains("gpt-5.3-codex"));
            assert!(request.contains("Hello Codex"));
            let body = r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"Hello from Codex."}]}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("mock server write failed");
        });
        let mut router = ModelRouter::empty();
        router.register_codex_with_base_url(
            "test-key",
            "gpt-5.3-codex",
            format!("http://{address}"),
        )?;
        let result = router.execute(&ModelCall {
            target: ModelTarget::Codex,
            prompt: "Hello Codex".to_string(),
        })?;
        assert_eq!(result.output, "Hello from Codex.");
        handle.join().map_err(|_| "mock server thread panicked")?;
        Ok(())
    }

    #[test]
    fn duplicate_backend_registration_is_rejected() -> Result<()> {
        let mut router = ModelRouter::empty();

        router.register(FakeBackend {
            target: ModelTarget::Claude,
            capabilities: vec![ModelCapability::GeneralResponse],
            response: "first".to_string(),
        })?;

        let result = router.register(FakeBackend {
            target: ModelTarget::Claude,
            capabilities: vec![ModelCapability::GeneralResponse],
            response: "second".to_string(),
        });

        assert!(result.is_err());

        Ok(())
    }

    #[test]
    fn qwen_forwards_json_response_format() -> Result<()> {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server accept failed");

            let mut buffer = [0u8; 8192];
            let size = stream.read(&mut buffer).expect("mock server read failed");
            let request = String::from_utf8_lossy(&buffer[..size]);

            let body = request.split_once("\r\n\r\n").expect("missing HTTP body").1;

            let body: serde_json::Value =
                serde_json::from_str(body).expect("request body must be valid JSON");

            assert_eq!(body["response_format"]["type"], "json_schema");
            assert_eq!(
                body["response_format"]["json_schema"]["schema"]["type"],
                "object"
            );
            assert_eq!(
                body["response_format"]["json_schema"]["schema"]["properties"]["entities"]["type"],
                "array"
            );

            let response_body = r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#;

            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
                response_body.len(),
                response_body
            );

            stream
                .write_all(response.as_bytes())
                .expect("mock server write failed");
        });

        let backend = QwenBackend::new(format!("http://{address}"), "Qwen3.5-4B-Q4_K_M.gguf");

        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "entities": {
                    "type": "array"
                }
            }
        });

        let output = backend.execute_with_response_format("test extraction", Some(&schema))?;

        assert_eq!(output, "{\"ok\":true}");

        handle.join().map_err(|_| "mock server thread panicked")?;

        Ok(())
    }

    #[test]
    fn model_router_forwards_json_response_format() -> Result<()> {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server accept failed");
            let mut buffer = [0u8; 8192];
            let size = stream.read(&mut buffer).expect("mock server read failed");
            let request = String::from_utf8_lossy(&buffer[..size]);

            assert!(request.starts_with("POST /v1/chat/completions"));
            assert!(request.contains("\"response_format\""));
            assert!(request.contains("\"json_schema\""));
            assert!(request.contains("\"strict\""));
            assert!(request.contains("\"schema\""));
            assert!(request.contains("\"router_test\""));

            let body = r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );

            stream
                .write_all(response.as_bytes())
                .expect("mock server write failed");
        });

        let router = ModelRouter::new(format!("http://{address}"), "Qwen3.5-4B-Q4_K_M.gguf");

        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["ok"],
            "properties": {
                "ok": {
                    "type": "boolean"
                },
                "router_test": {
                    "type": "string"
                }
            }
        });

        let result = router.execute_with_response_format(
            &ModelCall {
                target: ModelTarget::Qwen,
                prompt: "Return JSON.".to_string(),
            },
            Some(&schema),
        )?;

        assert_eq!(result.output, "{\"ok\":true}");

        handle.join().map_err(|_| "mock server thread panicked")?;

        Ok(())
    }
}
