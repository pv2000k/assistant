use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env,
    error::Error,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub const GOOGLE_CALENDAR_EVENTS_READONLY_SCOPE: &str =
    "https://www.googleapis.com/auth/calendar.events.readonly";
pub const GOOGLE_CALENDAR_EVENTS_SCOPE: &str = "https://www.googleapis.com/auth/calendar.events";

const DEFAULT_BASE_URL: &str = "https://www.googleapis.com/calendar/v3";
const GOOGLE_AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_MAX_RESULTS: u64 = 10;
const MAX_RESULTS: u64 = 50;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const ACCESS_TOKEN_REFRESH_SKEW: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCalendarToken {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_at: u64,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Clone)]
pub struct GoogleCalendarAuth {
    client: Client,
    client_id: Arc<str>,
    client_secret: Option<Arc<str>>,
    token_path: Arc<PathBuf>,
    auth_endpoint: Url,
    token_endpoint: Url,
    token: Arc<Mutex<Option<StoredCalendarToken>>>,
}

impl GoogleCalendarAuth {
    pub fn from_env() -> Result<Self> {
        let client_id = env::var("ASSISTANT_GOOGLE_CLIENT_ID").unwrap_or_default();
        let client_secret = env::var("ASSISTANT_GOOGLE_CLIENT_SECRET").ok();
        let token_path = if let Some(path) = env::var_os("ASSISTANT_GOOGLE_CALENDAR_TOKEN_PATH") {
            PathBuf::from(path)
        } else {
            default_google_calendar_token_path()?
        };

        Self::with_endpoints(
            client_id,
            client_secret,
            token_path,
            GOOGLE_AUTH_ENDPOINT,
            GOOGLE_TOKEN_ENDPOINT,
        )
    }

    pub fn new(
        client_id: impl Into<String>,
        client_secret: Option<String>,
        token_path: impl Into<PathBuf>,
    ) -> Result<Self> {
        Self::with_endpoints(
            client_id,
            client_secret,
            token_path,
            GOOGLE_AUTH_ENDPOINT,
            GOOGLE_TOKEN_ENDPOINT,
        )
    }

    pub fn with_endpoints(
        client_id: impl Into<String>,
        client_secret: Option<String>,
        token_path: impl Into<PathBuf>,
        auth_endpoint: &str,
        token_endpoint: &str,
    ) -> Result<Self> {
        let client = Client::builder().timeout(DEFAULT_TIMEOUT).build()?;
        let auth_endpoint = Url::parse(auth_endpoint)?;
        let token_endpoint = Url::parse(token_endpoint)?;
        let token_path = token_path.into();
        let token = load_calendar_token(&token_path)?;

        Ok(Self {
            client,
            client_id: Arc::<str>::from(client_id.into()),
            client_secret: client_secret.map(Arc::<str>::from),
            token_path: Arc::new(token_path),
            auth_endpoint,
            token_endpoint,
            token: Arc::new(Mutex::new(token)),
        })
    }

    pub fn has_client_id(&self) -> bool {
        !self.client_id.trim().is_empty()
    }

    pub fn is_authenticated(&self) -> bool {
        self.token
            .lock()
            .map(|token| token.is_some())
            .unwrap_or(false)
    }

    pub fn token_path(&self) -> &Path {
        self.token_path.as_path()
    }

    pub fn has_event_write_scope(&self) -> Result<bool> {
        let token = self
            .token
            .lock()
            .map_err(|_| "Google Calendar credential state was poisoned.")?;

        Ok(token
            .as_ref()
            .and_then(|token| token.scope.as_deref())
            .map(scope_grants_event_write)
            .unwrap_or(false))
    }

    pub fn login(&self) -> Result<()> {
        if !self.has_client_id() {
            return Err(
                "Google Calendar OAuth requires ASSISTANT_GOOGLE_CLIENT_ID. Create a Desktop OAuth client and set the client ID first.".into(),
            );
        }

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let redirect_uri = format!("http://127.0.0.1:{}/oauth2callback", address.port());

        let state = random_urlsafe(32)?;
        let code_verifier = random_urlsafe(64)?;
        let code_challenge = code_challenge(&code_verifier);

        let mut authorization_url = self.auth_endpoint.clone();
        {
            let mut query = authorization_url.query_pairs_mut();
            query.append_pair("client_id", &self.client_id);
            query.append_pair("response_type", "code");
            query.append_pair("redirect_uri", &redirect_uri);
            query.append_pair("scope", GOOGLE_CALENDAR_EVENTS_SCOPE);
            query.append_pair("access_type", "offline");
            query.append_pair("prompt", "consent");
            query.append_pair("state", &state);
            query.append_pair("code_challenge", &code_challenge);
            query.append_pair("code_challenge_method", "S256");
        }

        println!("Google Calendar authorization required.");
        println!("Opening the Google authorization page in your browser...");
        println!("Authorization URL: {}", authorization_url);
        open_browser(&authorization_url);

        let (code, returned_state) = receive_oauth_callback(listener)?;

        if returned_state != state {
            return Err("Google Calendar OAuth state validation failed.".into());
        }

        let token = self.exchange_code(&code, &code_verifier, &redirect_uri)?;
        if token.refresh_token.is_none() {
            return Err(
                "Google OAuth did not return a refresh token. Re-run :calendar-login and approve offline access.".into(),
            );
        }

        self.store_token(token)?;
        println!("Google Calendar authorization saved.");
        Ok(())
    }

    pub fn access_token(&self) -> Result<String> {
        let mut guard = self
            .token
            .lock()
            .map_err(|_| "Google Calendar credential state was poisoned.")?;

        let token = guard
            .as_ref()
            .ok_or("Google Calendar is not authenticated. Run :calendar-login first.")?;

        if token.expires_at > now_unix().saturating_add(ACCESS_TOKEN_REFRESH_SKEW) {
            return Ok(token.access_token.clone());
        }

        let refreshed = self.refresh_token(token)?;
        let access_token = refreshed.access_token.clone();
        *guard = Some(refreshed);
        drop(guard);
        self.persist_current_token()?;

        Ok(access_token)
    }

    pub fn force_refresh(&self) -> Result<String> {
        let mut guard = self
            .token
            .lock()
            .map_err(|_| "Google Calendar credential state was poisoned.")?;

        let token = guard
            .as_ref()
            .ok_or("Google Calendar is not authenticated. Run :calendar-login first.")?;

        let refreshed = self.refresh_token(token)?;
        let access_token = refreshed.access_token.clone();
        *guard = Some(refreshed);
        drop(guard);
        self.persist_current_token()?;

        Ok(access_token)
    }

    fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<StoredCalendarToken> {
        let mut form = vec![
            ("client_id", self.client_id.to_string()),
            ("code", code.to_string()),
            ("code_verifier", code_verifier.to_string()),
            ("grant_type", "authorization_code".to_string()),
            ("redirect_uri", redirect_uri.to_string()),
        ];

        if let Some(client_secret) = &self.client_secret {
            form.push(("client_secret", client_secret.to_string()));
        }

        let response = self
            .client
            .post(self.token_endpoint.clone())
            .form(&form)
            .send()?;
        parse_token_response(response, None)
    }

    fn refresh_token(&self, current: &StoredCalendarToken) -> Result<StoredCalendarToken> {
        let refresh_token = current
            .refresh_token
            .as_deref()
            .ok_or("Google Calendar refresh token is missing. Run :calendar-login again.")?;

        if !self.has_client_id() {
            return Err(
                "Google Calendar access token expired and ASSISTANT_GOOGLE_CLIENT_ID is not configured, so it cannot be refreshed.".into(),
            );
        }

        let mut form = vec![
            ("client_id", self.client_id.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.to_string()),
        ];

        if let Some(client_secret) = &self.client_secret {
            form.push(("client_secret", client_secret.to_string()));
        }

        let response = self
            .client
            .post(self.token_endpoint.clone())
            .form(&form)
            .send()?;
        parse_token_response(response, Some(current))
    }

    fn store_token(&self, token: StoredCalendarToken) -> Result<()> {
        let parent = self
            .token_path
            .parent()
            .ok_or("Google Calendar token path has no parent directory.")?;

        fs::create_dir_all(parent)?;

        let temporary_path = self
            .token_path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(&token)?;
        fs::write(&temporary_path, bytes)?;
        set_private_permissions(&temporary_path)?;
        fs::rename(&temporary_path, &*self.token_path)?;

        let mut guard = self
            .token
            .lock()
            .map_err(|_| "Google Calendar credential state was poisoned.")?;
        *guard = Some(token);
        Ok(())
    }

    fn persist_current_token(&self) -> Result<()> {
        let token = self
            .token
            .lock()
            .map_err(|_| "Google Calendar credential state was poisoned.")?
            .clone()
            .ok_or("Google Calendar is not authenticated.")?;

        let parent = self
            .token_path
            .parent()
            .ok_or("Google Calendar token path has no parent directory.")?;
        fs::create_dir_all(parent)?;

        let temporary_path = self
            .token_path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        fs::write(&temporary_path, serde_json::to_vec_pretty(&token)?)?;
        set_private_permissions(&temporary_path)?;
        fs::rename(&temporary_path, &*self.token_path)?;
        Ok(())
    }
}

fn default_google_calendar_token_path() -> Result<PathBuf> {
    let config_root = if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(value)
    } else {
        let home = env::var_os("HOME").ok_or("HOME environment variable is not set.")?;
        PathBuf::from(home).join(".config")
    };

    Ok(config_root
        .join("assistant")
        .join("google-calendar-token.json"))
}

fn load_calendar_token(path: &Path) -> Result<Option<StoredCalendarToken>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(serde_json::from_str(&contents)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_urlsafe(byte_count: usize) -> Result<String> {
    let mut bytes = vec![0u8; byte_count];
    let mut rng = OsRng;
    rng.fill_bytes(&mut bytes);
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn code_challenge(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn open_browser(url: &Url) {
    let url = url.as_str();
    if Command::new("xdg-open").arg(url).spawn().is_err() {
        println!("Could not open the browser automatically. Open the URL above manually.");
    }
}

fn receive_oauth_callback(listener: TcpListener) -> Result<(String, String)> {
    let (mut stream, _) = listener.accept()?;
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];

    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let first_line = String::from_utf8_lossy(&request)
        .lines()
        .next()
        .ok_or("Google OAuth callback was empty.")?
        .to_string();
    let mut parts = first_line.split_whitespace();
    let method = parts
        .next()
        .ok_or("Google OAuth callback method missing.")?;
    let target = parts
        .next()
        .ok_or("Google OAuth callback target missing.")?;

    if method != "GET" {
        write_callback_response(
            &mut stream,
            "Google Calendar authorization failed: invalid method.",
        )?;
        return Err("Google OAuth callback must use GET.".into());
    }

    let callback_url = Url::parse(&format!("http://127.0.0.1{target}"))?;

    if callback_url.path() != "/oauth2callback" {
        write_callback_response(
            &mut stream,
            "Google Calendar authorization failed: invalid callback path.",
        )?;
        return Err("Google OAuth callback path was invalid.".into());
    }

    if let Some(error) = callback_url
        .query_pairs()
        .find(|(key, _)| key == "error")
        .map(|(_, value)| value.into_owned())
    {
        write_callback_response(&mut stream, "Google Calendar authorization was denied.")?;
        return Err(format!("Google OAuth authorization failed: {error}.").into());
    }

    let code = callback_url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .ok_or("Google OAuth callback did not contain an authorization code.")?;

    let state = callback_url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .ok_or("Google OAuth callback did not contain state.")?;

    write_callback_response(
        &mut stream,
        "Google Calendar authorization completed. You can close this browser tab and return to the assistant.",
    )?;

    Ok((code, state))
}

fn write_callback_response(stream: &mut TcpStream, message: &str) -> Result<()> {
    let body = format!(
        "<!doctype html><html><body><h2>{}</h2><p>You can close this tab.</p></body></html>",
        message
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn parse_token_response(
    response: Response,
    previous: Option<&StoredCalendarToken>,
) -> Result<StoredCalendarToken> {
    let status = response.status();
    let body = response.text()?;
    let parsed: GoogleTokenResponse = serde_json::from_str(&body).map_err(|_| {
        format!(
            "Google OAuth token endpoint returned HTTP {} with an invalid response.",
            status
        )
    })?;

    if !status.is_success() || parsed.error.is_some() {
        let message = parsed
            .error_description
            .or(parsed.error)
            .unwrap_or_else(|| "unknown OAuth error".to_string());
        return Err(format!("Google OAuth token request failed: {message}.").into());
    }

    let access_token = parsed
        .access_token
        .ok_or("Google OAuth token response did not contain an access token.")?;
    let refresh_token = parsed
        .refresh_token
        .or_else(|| previous.and_then(|token| token.refresh_token.clone()));
    let expires_in = parsed.expires_in.unwrap_or(3600);
    let scope = parsed
        .scope
        .or_else(|| previous.and_then(|token| token.scope.clone()));

    Ok(StoredCalendarToken {
        access_token,
        refresh_token,
        expires_at: now_unix().saturating_add(expires_in),
        scope,
    })
}

#[derive(Clone)]
enum CalendarCredentialSource {
    Static(Arc<str>),
    OAuth(GoogleCalendarAuth),
}

impl std::fmt::Debug for CalendarCredentialSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(_) => formatter.write_str("Static(<redacted>)"),
            Self::OAuth(_) => formatter.write_str("OAuth(<redacted>)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GoogleCalendarClient {
    credentials: CalendarCredentialSource,
    default_calendar_id: Arc<str>,
    base_url: Url,
    client: Client,
}

impl GoogleCalendarClient {
    pub fn new(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Self::with_base_url(access_token, default_calendar_id, DEFAULT_BASE_URL)
    }

    pub fn with_base_url(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
        base_url: &str,
    ) -> Result<Self> {
        let access_token = access_token.into();
        if access_token.trim().is_empty() {
            return Err("Google Calendar access token cannot be empty.".into());
        }

        let default_calendar_id = default_calendar_id.into();
        if default_calendar_id.trim().is_empty() {
            return Err("Google Calendar default calendar ID cannot be empty.".into());
        }

        let client = Client::builder().timeout(DEFAULT_TIMEOUT).build()?;
        let mut parsed_base_url = Url::parse(base_url)?;

        let normalized_path = parsed_base_url.path().trim_end_matches('/').to_string();

        parsed_base_url.set_path(&normalized_path);

        Ok(Self {
            credentials: CalendarCredentialSource::Static(Arc::<str>::from(access_token)),
            default_calendar_id: Arc::<str>::from(default_calendar_id),
            base_url: parsed_base_url,
            client,
        })
    }

    pub fn with_auth(
        auth: GoogleCalendarAuth,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Self::with_auth_base_url(auth, default_calendar_id, DEFAULT_BASE_URL)
    }

    pub fn with_auth_base_url(
        auth: GoogleCalendarAuth,
        default_calendar_id: impl Into<String>,
        base_url: &str,
    ) -> Result<Self> {
        let default_calendar_id = default_calendar_id.into();
        if default_calendar_id.trim().is_empty() {
            return Err("Google Calendar default calendar ID cannot be empty.".into());
        }

        let client = Client::builder().timeout(DEFAULT_TIMEOUT).build()?;
        let mut parsed_base_url = Url::parse(base_url)?;
        let normalized_path = parsed_base_url.path().trim_end_matches('/').to_string();
        parsed_base_url.set_path(&normalized_path);

        Ok(Self {
            credentials: CalendarCredentialSource::OAuth(auth),
            default_calendar_id: Arc::<str>::from(default_calendar_id),
            base_url: parsed_base_url,
            client,
        })
    }

    fn calendar_id(&self, arguments: &Value) -> Result<String> {
        match arguments.get("calendar_id") {
            Some(value) => {
                let calendar_id = value.as_str().ok_or("calendar_id must be a string.")?;
                if calendar_id.trim().is_empty() {
                    return Err("calendar_id cannot be empty.".into());
                }
                Ok(calendar_id.to_string())
            }
            None => Ok(self.default_calendar_id.to_string()),
        }
    }

    fn events_url(&self, calendar_id: &str, event_id: Option<&str>) -> Result<Url> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| "Google Calendar base URL cannot be a base URL.")?;
            segments.push("calendars");
            segments.push(calendar_id);
            segments.push("events");
            if let Some(event_id) = event_id {
                segments.push(event_id);
            }
        }
        Ok(url)
    }

    fn send_get(&self, url: Url) -> Result<Value> {
        let token = match &self.credentials {
            CalendarCredentialSource::Static(token) => token.to_string(),
            CalendarCredentialSource::OAuth(auth) => auth.access_token()?,
        };

        let mut response = self.perform_get(url.clone(), &token)?;

        if response.status() == StatusCode::UNAUTHORIZED {
            if let CalendarCredentialSource::OAuth(auth) = &self.credentials {
                let refreshed_token = auth.force_refresh()?;
                response = self.perform_get(url, &refreshed_token)?;
            }
        }

        let status = response.status();
        let body = response.text()?;

        if !status.is_success() {
            return Err(format!(
                "Google Calendar returned HTTP {}: {}",
                status,
                truncate_for_error(&body)
            )
            .into());
        }

        Ok(serde_json::from_str(&body)?)
    }

    fn perform_get(&self, url: Url, token: &str) -> Result<Response> {
        Ok(self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()?)
    }

    fn perform_mutation(
        &self,
        method: Method,
        url: Url,
        token: &str,
        body: Option<&Value>,
    ) -> Result<Response> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(token)
            .header("Accept", "application/json");

        if let Some(body) = body {
            request = request.json(body);
        }

        Ok(request.send()?)
    }

    fn ensure_event_write_access(&self) -> Result<()> {
        match &self.credentials {
            CalendarCredentialSource::Static(_) => Ok(()),
            CalendarCredentialSource::OAuth(auth) => {
                if !auth.is_authenticated() {
                    return Err(
                        "Google Calendar is not authenticated. Run :calendar-login first.".into(),
                    );
                }

                if !auth.has_event_write_scope()? {
                    return Err(
                        "Google Calendar credentials do not have event write access. Run :calendar-login again to grant calendar.events access.".into(),
                    );
                }

                Ok(())
            }
        }
    }

    fn send_mutation(
        &self,
        method: Method,
        url: Url,
        body: Option<&Value>,
    ) -> Result<Option<Value>> {
        self.ensure_event_write_access()?;

        let token = match &self.credentials {
            CalendarCredentialSource::Static(token) => token.to_string(),
            CalendarCredentialSource::OAuth(auth) => auth.access_token()?,
        };

        let mut response = self.perform_mutation(method.clone(), url.clone(), &token, body)?;

        if response.status() == StatusCode::UNAUTHORIZED {
            if let CalendarCredentialSource::OAuth(auth) = &self.credentials {
                let refreshed_token = auth.force_refresh()?;
                response = self.perform_mutation(method, url, &refreshed_token, body)?;
            }
        }

        let status = response.status();
        let response_body = response.text()?;

        if !status.is_success() {
            return Err(format!(
                "Google Calendar returned HTTP {}: {}",
                status,
                truncate_for_error(&response_body)
            )
            .into());
        }

        if response_body.trim().is_empty() {
            return Ok(None);
        }

        Ok(Some(serde_json::from_str(&response_body)?))
    }

    pub fn list_events(&self, arguments: &Value) -> Result<ToolResultData> {
        let object = arguments
            .as_object()
            .ok_or("calendar.list_events arguments must be an object.")?;

        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "calendar_id" | "time_min" | "time_max" | "query" | "max_results" | "page_token"
            ) {
                return Err(
                    format!("calendar.list_events does not accept argument '{key}'.").into(),
                );
            }
        }

        let calendar_id = self.calendar_id(arguments)?;
        let max_results = object
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_RESULTS);
        if !(1..=MAX_RESULTS).contains(&max_results) {
            return Err(format!("max_results must be 1..={MAX_RESULTS}.").into());
        }

        let time_min = optional_nonempty_string(object.get("time_min"), "time_min")?;
        let time_max = optional_nonempty_string(object.get("time_max"), "time_max")?;
        let query = optional_nonempty_string(object.get("query"), "query")?;
        let page_token = optional_nonempty_string(object.get("page_token"), "page_token")?;

        let mut url = self.events_url(&calendar_id, None)?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("maxResults", &max_results.to_string());
            pairs.append_pair("singleEvents", "true");
            pairs.append_pair("orderBy", "startTime");
            pairs.append_pair("showDeleted", "false");
            if let Some(time_min) = time_min.as_deref() {
                pairs.append_pair("timeMin", time_min);
            }
            if let Some(time_max) = time_max.as_deref() {
                pairs.append_pair("timeMax", time_max);
            }
            if let Some(query) = query.as_deref() {
                pairs.append_pair("q", query);
            }
            if let Some(page_token) = page_token.as_deref() {
                pairs.append_pair("pageToken", page_token);
            }
        }

        let body = self.send_get(url)?;
        let events = body
            .get("items")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(compact_event).collect::<Vec<_>>())
            .unwrap_or_default();

        Ok(ToolResultData {
            output: serde_json::json!({
                "calendar_id": calendar_id,
                "events": events,
                "count": events.len(),
                "next_page_token": body.get("nextPageToken").cloned().unwrap_or(Value::Null),
                "time_zone": body.get("timeZone").cloned().unwrap_or(Value::Null)
            }),
        })
    }

    pub fn get_event(&self, arguments: &Value) -> Result<ToolResultData> {
        let object = arguments
            .as_object()
            .ok_or("calendar.get_event arguments must be an object.")?;

        for key in object.keys() {
            if !matches!(key.as_str(), "calendar_id" | "event_id") {
                return Err(format!("calendar.get_event does not accept argument '{key}'.").into());
            }
        }

        let calendar_id = self.calendar_id(arguments)?;
        let event_id = object
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or("calendar.get_event requires event_id.")?;
        if event_id.trim().is_empty() {
            return Err("event_id cannot be empty.".into());
        }

        let url = self.events_url(&calendar_id, Some(event_id))?;
        let body = self.send_get(url)?;

        Ok(ToolResultData {
            output: serde_json::json!({
                "calendar_id": calendar_id,
                "event": compact_event(&body)
            }),
        })
    }

    pub fn create_event(&self, arguments: &Value) -> Result<ToolResultData> {
        let (calendar_id, event, send_updates) = build_create_event(arguments)?;
        let calendar_id = if calendar_id.is_empty() {
            self.default_calendar_id.to_string()
        } else {
            calendar_id
        };
        let mut url = self.events_url(&calendar_id, None)?;
        url.query_pairs_mut()
            .append_pair("sendUpdates", &send_updates);

        let body = self
            .send_mutation(Method::POST, url, Some(&event))?
            .ok_or("Google Calendar create event returned an empty response.")?;

        Ok(ToolResultData {
            output: serde_json::json!({
                "calendar_id": calendar_id,
                "event": compact_event(&body)
            }),
        })
    }

    pub fn update_event(&self, arguments: &Value) -> Result<ToolResultData> {
        let (calendar_id, event_id, patch, send_updates) = build_update_event(arguments)?;
        let calendar_id = if calendar_id.is_empty() {
            self.default_calendar_id.to_string()
        } else {
            calendar_id
        };
        let mut url = self.events_url(&calendar_id, Some(&event_id))?;
        url.query_pairs_mut()
            .append_pair("sendUpdates", &send_updates);

        let body = self
            .send_mutation(Method::PATCH, url, Some(&patch))?
            .ok_or("Google Calendar update event returned an empty response.")?;

        Ok(ToolResultData {
            output: serde_json::json!({
                "calendar_id": calendar_id,
                "event": compact_event(&body)
            }),
        })
    }
}

fn scope_grants_event_write(scope: &str) -> bool {
    scope.split_whitespace().any(|value| {
        matches!(
            value,
            GOOGLE_CALENDAR_EVENTS_SCOPE | "https://www.googleapis.com/auth/calendar"
        )
    })
}

fn validate_event_time(value: &Value, name: &str) -> Result<Value> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{name} must be an object."))?;

    for key in object.keys() {
        if !matches!(key.as_str(), "date" | "dateTime" | "timeZone") {
            return Err(format!("{name} does not accept '{key}'.").into());
        }
    }

    let has_date = object
        .get("date")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    let has_date_time = object
        .get("dateTime")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());

    if has_date == has_date_time {
        return Err(format!("{name} must contain exactly one non-empty date or dateTime.").into());
    }

    if has_date && object.contains_key("timeZone") {
        return Err(format!("{name} timeZone is only valid with dateTime.").into());
    }

    if let Some(time_zone) = object.get("timeZone") {
        if !time_zone
            .as_str()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(format!("{name} timeZone must be a non-empty string.").into());
        }
    }

    Ok(value.clone())
}

fn validate_attendees(value: &Value) -> Result<Value> {
    let attendees = value
        .as_array()
        .ok_or("attendees must be an array of email addresses.")?;

    if attendees.len() > 100 {
        return Err("attendees accepts at most 100 email addresses.".into());
    }

    let mut normalized = Vec::with_capacity(attendees.len());
    for attendee in attendees {
        let email = attendee
            .as_str()
            .ok_or("attendees must contain only email address strings.")?;
        if email.trim().is_empty() || email.chars().any(char::is_whitespace) {
            return Err("attendees contains an invalid email address.".into());
        }
        normalized.push(serde_json::json!({"email": email}));
    }

    Ok(Value::Array(normalized))
}

fn validate_recurrence(value: &Value) -> Result<Value> {
    let recurrence = value
        .as_array()
        .ok_or("recurrence must be an array of RRULE strings.")?;

    if recurrence.len() > 10 {
        return Err("recurrence accepts at most 10 rules.".into());
    }

    let mut normalized = Vec::with_capacity(recurrence.len());
    for rule in recurrence {
        let rule = rule
            .as_str()
            .ok_or("recurrence must contain only strings.")?;
        if rule.trim().is_empty() {
            return Err("recurrence cannot contain empty strings.".into());
        }
        normalized.push(Value::String(rule.to_string()));
    }

    Ok(Value::Array(normalized))
}

fn validate_send_updates(value: Option<&Value>) -> Result<String> {
    let send_updates = value.and_then(Value::as_str).unwrap_or("none").to_string();

    if !matches!(send_updates.as_str(), "all" | "externalOnly" | "none") {
        return Err("send_updates must be one of all, externalOnly, or none.".into());
    }

    Ok(send_updates)
}

fn validate_event_fields(
    object: &serde_json::Map<String, Value>,
    allow_empty_strings: bool,
) -> Result<Value> {
    let mut event = serde_json::Map::new();

    if let Some(summary) = object.get("summary") {
        let summary = summary.as_str().ok_or("summary must be a string.")?;
        if !allow_empty_strings && summary.trim().is_empty() {
            return Err("summary cannot be empty.".into());
        }
        event.insert("summary".to_string(), Value::String(summary.to_string()));
    }

    for field in ["description", "location"] {
        if let Some(value) = object.get(field) {
            let value = value
                .as_str()
                .ok_or_else(|| format!("{field} must be a string."))?;
            if !allow_empty_strings && value.trim().is_empty() {
                return Err(format!("{field} cannot be empty.").into());
            }
            event.insert(field.to_string(), Value::String(value.to_string()));
        }
    }

    if let Some(start) = object.get("start") {
        event.insert("start".to_string(), validate_event_time(start, "start")?);
    }
    if let Some(end) = object.get("end") {
        event.insert("end".to_string(), validate_event_time(end, "end")?);
    }
    if let Some(attendees) = object.get("attendees") {
        event.insert("attendees".to_string(), validate_attendees(attendees)?);
    }
    if let Some(recurrence) = object.get("recurrence") {
        event.insert("recurrence".to_string(), validate_recurrence(recurrence)?);
    }

    Ok(Value::Object(event))
}

fn optional_calendar_id(object: &serde_json::Map<String, Value>, name: &str) -> Result<String> {
    match object.get("calendar_id") {
        None => Ok(String::new()),
        Some(value) => {
            let calendar_id = value
                .as_str()
                .ok_or_else(|| format!("{name} calendar_id must be a string."))?;
            if calendar_id.trim().is_empty() {
                return Err(format!("{name} calendar_id cannot be empty.").into());
            }
            Ok(calendar_id.to_string())
        }
    }
}

fn build_create_event(arguments: &Value) -> Result<(String, Value, String)> {
    let object = arguments
        .as_object()
        .ok_or("calendar.create_event arguments must be an object.")?;

    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "calendar_id"
                | "summary"
                | "description"
                | "location"
                | "start"
                | "end"
                | "attendees"
                | "recurrence"
                | "send_updates"
        ) {
            return Err(format!("calendar.create_event does not accept argument '{key}'.").into());
        }
    }

    let calendar_id = optional_calendar_id(object, "calendar.create_event")?;
    let summary = object
        .get("summary")
        .and_then(Value::as_str)
        .ok_or("calendar.create_event requires a string summary.")?;
    if summary.trim().is_empty() {
        return Err("calendar.create_event summary cannot be empty.".into());
    }
    if !object.contains_key("start") || !object.contains_key("end") {
        return Err("calendar.create_event requires start and end.".into());
    }

    let event = validate_event_fields(object, false)?;
    let send_updates = validate_send_updates(object.get("send_updates"))?;

    Ok((calendar_id, event, send_updates))
}

fn build_update_event(arguments: &Value) -> Result<(String, String, Value, String)> {
    let object = arguments
        .as_object()
        .ok_or("calendar.update_event arguments must be an object.")?;

    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "calendar_id"
                | "event_id"
                | "summary"
                | "description"
                | "location"
                | "start"
                | "end"
                | "attendees"
                | "recurrence"
                | "send_updates"
        ) {
            return Err(format!("calendar.update_event does not accept argument '{key}'.").into());
        }
    }

    let calendar_id = optional_calendar_id(object, "calendar.update_event")?;
    let event_id = object
        .get("event_id")
        .and_then(Value::as_str)
        .ok_or("calendar.update_event requires event_id.")?;
    if event_id.trim().is_empty() {
        return Err("calendar.update_event event_id cannot be empty.".into());
    }

    let mut event_fields = object.clone();
    event_fields.remove("calendar_id");
    event_fields.remove("event_id");
    event_fields.remove("send_updates");

    if event_fields.is_empty() {
        return Err("calendar.update_event requires at least one event field to change.".into());
    }

    let patch = validate_event_fields(&event_fields, true)?;
    let send_updates = validate_send_updates(object.get("send_updates"))?;

    Ok((calendar_id, event_id.to_string(), patch, send_updates))
}

fn optional_nonempty_string(value: Option<&Value>, name: &str) -> Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("{name} must be a string."))?;
            if text.trim().is_empty() {
                return Err(format!("{name} cannot be empty.").into());
            }
            Ok(Some(text.to_string()))
        }
    }
}

fn compact_event(event: &Value) -> Value {
    serde_json::json!({
        "id": event.get("id").cloned().unwrap_or(Value::Null),
        "status": event.get("status").cloned().unwrap_or(Value::Null),
        "summary": event.get("summary").cloned().unwrap_or(Value::Null),
        "description": event.get("description").cloned().unwrap_or(Value::Null),
        "location": event.get("location").cloned().unwrap_or(Value::Null),
        "start": event.get("start").cloned().unwrap_or(Value::Null),
        "end": event.get("end").cloned().unwrap_or(Value::Null),
        "html_link": event.get("htmlLink").cloned().unwrap_or(Value::Null),
        "event_type": event.get("eventType").cloned().unwrap_or(Value::Null),
        "organizer": event.get("organizer").cloned().unwrap_or(Value::Null),
        "attendees": event.get("attendees").cloned().unwrap_or(Value::Null)
    })
}

fn truncate_for_error(body: &str) -> String {
    body.chars().take(1000).collect()
}

#[derive(Debug, Clone)]
pub struct ToolResultData {
    pub output: Value,
}

pub struct GoogleCalendarListEventsTool {
    client: GoogleCalendarClient,
}

impl GoogleCalendarListEventsTool {
    pub fn new(client: GoogleCalendarClient) -> Self {
        Self { client }
    }

    pub fn with_credentials(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::new(
            access_token,
            default_calendar_id,
        )?))
    }

    pub fn with_base_url(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
        base_url: &str,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::with_base_url(
            access_token,
            default_calendar_id,
            base_url,
        )?))
    }

    pub fn with_auth(
        auth: GoogleCalendarAuth,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::with_auth(
            auth,
            default_calendar_id,
        )?))
    }
}

impl crate::Tool for GoogleCalendarListEventsTool {
    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition {
            name: "calendar.list_events".to_string(),
            description: "List Google Calendar events visible to the connected account."
                .to_string(),
            permission: crate::ToolPermission::ReadOnly,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<crate::ToolResult> {
        Ok(crate::ToolResult {
            success: true,
            output: self.client.list_events(arguments)?.output,
        })
    }
}

pub struct GoogleCalendarGetEventTool {
    client: GoogleCalendarClient,
}

impl GoogleCalendarGetEventTool {
    pub fn new(client: GoogleCalendarClient) -> Self {
        Self { client }
    }

    pub fn with_credentials(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::new(
            access_token,
            default_calendar_id,
        )?))
    }

    pub fn with_base_url(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
        base_url: &str,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::with_base_url(
            access_token,
            default_calendar_id,
            base_url,
        )?))
    }

    pub fn with_auth(
        auth: GoogleCalendarAuth,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::with_auth(
            auth,
            default_calendar_id,
        )?))
    }
}

impl crate::Tool for GoogleCalendarGetEventTool {
    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition {
            name: "calendar.get_event".to_string(),
            description: "Get one Google Calendar event visible to the connected account."
                .to_string(),
            permission: crate::ToolPermission::ReadOnly,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<crate::ToolResult> {
        Ok(crate::ToolResult {
            success: true,
            output: self.client.get_event(arguments)?.output,
        })
    }
}

pub struct GoogleCalendarCreateEventTool {
    client: GoogleCalendarClient,
}

impl GoogleCalendarCreateEventTool {
    pub fn new(client: GoogleCalendarClient) -> Self {
        Self { client }
    }

    pub fn with_credentials(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::new(
            access_token,
            default_calendar_id,
        )?))
    }

    pub fn with_auth(
        auth: GoogleCalendarAuth,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::with_auth(
            auth,
            default_calendar_id,
        )?))
    }

    pub fn with_base_url(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
        base_url: &str,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::with_base_url(
            access_token,
            default_calendar_id,
            base_url,
        )?))
    }
}

impl crate::Tool for GoogleCalendarCreateEventTool {
    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition {
            name: "calendar.create_event".to_string(),
            description: "Create a Google Calendar event. Requires user approval.".to_string(),
            permission: crate::ToolPermission::ApprovalRequired,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<crate::ToolResult> {
        Ok(crate::ToolResult {
            success: true,
            output: self.client.create_event(arguments)?.output,
        })
    }
}

pub struct GoogleCalendarUpdateEventTool {
    client: GoogleCalendarClient,
}

impl GoogleCalendarUpdateEventTool {
    pub fn new(client: GoogleCalendarClient) -> Self {
        Self { client }
    }

    pub fn with_credentials(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::new(
            access_token,
            default_calendar_id,
        )?))
    }

    pub fn with_auth(
        auth: GoogleCalendarAuth,
        default_calendar_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::with_auth(
            auth,
            default_calendar_id,
        )?))
    }

    pub fn with_base_url(
        access_token: impl Into<String>,
        default_calendar_id: impl Into<String>,
        base_url: &str,
    ) -> Result<Self> {
        Ok(Self::new(GoogleCalendarClient::with_base_url(
            access_token,
            default_calendar_id,
            base_url,
        )?))
    }
}

impl crate::Tool for GoogleCalendarUpdateEventTool {
    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition {
            name: "calendar.update_event".to_string(),
            description: "Update a Google Calendar event. Requires user approval.".to_string(),
            permission: crate::ToolPermission::ApprovalRequired,
        }
    }

    fn execute(&self, arguments: &Value) -> Result<crate::ToolResult> {
        Ok(crate::ToolResult {
            success: true,
            output: self.client.update_event(arguments)?.output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn start_mock_server(response_body: &str) -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let body = response_body.to_string();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server accept failed");
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            loop {
                let read = stream.read(&mut buffer).expect("mock server read failed");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("GET /calendar/v3/calendars/primary/events"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-token")
            );

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("mock server write failed");
        });
        Ok((format!("http://{address}/calendar/v3"), handle))
    }

    #[test]
    fn list_events_is_read_only_and_parses_google_response() -> Result<()> {
        let (base_url, handle) = start_mock_server(
            r#"{
                "timeZone": "Asia/Kolkata",
                "items": [
                    {
                        "id": "evt-1",
                        "summary": "Project sync",
                        "start": {"dateTime": "2026-09-12T18:00:00+05:30"},
                        "end": {"dateTime": "2026-09-12T18:30:00+05:30"},
                        "htmlLink": "https://calendar.google.com/event"
                    }
                ],
                "nextPageToken": "next-token"
            }"#,
        )?;

        let tool = GoogleCalendarListEventsTool::with_base_url("test-token", "primary", &base_url)?;

        assert_eq!(
            tool.definition().permission,
            crate::ToolPermission::ReadOnly
        );

        let result = tool.execute(&serde_json::json!({
            "time_min": "2026-09-12T00:00:00Z",
            "max_results": 10
        }))?;

        assert!(result.success);
        assert_eq!(result.output["events"][0]["summary"], "Project sync");
        assert_eq!(result.output["next_page_token"], "next-token");
        assert_eq!(result.output["time_zone"], "Asia/Kolkata");

        handle.join().map_err(|_| "mock server thread panicked")?;
        Ok(())
    }

    #[test]
    fn get_event_is_read_only() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server accept failed");
            let mut request = [0u8; 4096];
            let size = stream.read(&mut request).expect("mock server read failed");
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /calendar/v3/calendars/primary/events/evt-1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-token")
            );
            let body = r#"{"id":"evt-1","summary":"Project sync","start":{"dateTime":"2026-09-12T18:00:00+05:30"},"end":{"dateTime":"2026-09-12T18:30:00+05:30"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("mock server write failed");
        });

        let tool = GoogleCalendarGetEventTool::with_base_url(
            "test-token",
            "primary",
            &format!("http://{address}/calendar/v3"),
        )?;
        assert_eq!(
            tool.definition().permission,
            crate::ToolPermission::ReadOnly
        );
        let result = tool.execute(&serde_json::json!({"event_id": "evt-1"}))?;
        assert!(result.success);
        assert_eq!(result.output["event"]["summary"], "Project sync");
        handle.join().map_err(|_| "mock server thread panicked")?;
        Ok(())
    }

    fn start_request_mock_server(
        expected_method: &str,
        expected_path_prefix: &str,
        expected_body_fragment: Option<&str>,
        status: &str,
        response_body: &str,
    ) -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let expected_method = expected_method.to_string();
        let expected_path_prefix = expected_path_prefix.to_string();
        let expected_body_fragment = expected_body_fragment.map(str::to_string);
        let response_body = response_body.to_string();

        let status = status.to_string();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server accept failed");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];

            loop {
                let read = stream.read(&mut buffer).expect("mock server read failed");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);

                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };

                let header_end = header_end + 4;
                let header_text = String::from_utf8_lossy(&request[..header_end]);
                let content_length = header_text
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);

                if request.len() >= header_end + content_length {
                    break;
                }
            }

            let request_text = String::from_utf8_lossy(&request);
            assert!(
                request_text.starts_with(&format!("{} {}", expected_method, expected_path_prefix)),
                "unexpected request: {request_text}"
            );

            if let Some(expected_body_fragment) = expected_body_fragment.as_deref() {
                assert!(
                    request_text.contains(expected_body_fragment),
                    "expected body fragment '{expected_body_fragment}' in request: {request_text}"
                );
            }

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .expect("mock server write failed");
        });

        Ok((format!("http://{address}/calendar/v3"), handle))
    }

    #[test]
    fn mutation_tools_have_expected_permissions() -> Result<()> {
        let create = GoogleCalendarCreateEventTool::with_credentials("test-token", "primary")?;
        let update = GoogleCalendarUpdateEventTool::with_credentials("test-token", "primary")?;

        assert_eq!(
            create.definition().permission,
            crate::ToolPermission::ApprovalRequired
        );
        assert_eq!(
            update.definition().permission,
            crate::ToolPermission::ApprovalRequired
        );
        Ok(())
    }

    #[test]
    fn create_event_posts_expected_request() -> Result<()> {
        let (base_url, handle) = start_request_mock_server(
            "POST",
            "/calendar/v3/calendars/primary/events?sendUpdates=none",
            Some("\"summary\":\"Project sync\""),
            "200 OK",
            r#"{"id":"evt-1","summary":"Project sync","start":{"dateTime":"2026-09-12T18:00:00+05:30"},"end":{"dateTime":"2026-09-12T18:30:00+05:30"}}"#,
        )?;

        let tool =
            GoogleCalendarCreateEventTool::with_base_url("test-token", "primary", &base_url)?;

        let result = tool.execute(&serde_json::json!({
            "summary": "Project sync",
            "start": {"dateTime": "2026-09-12T18:00:00+05:30"},
            "end": {"dateTime": "2026-09-12T18:30:00+05:30"}
        }))?;

        assert!(result.success);
        assert_eq!(result.output["event"]["id"], "evt-1");
        handle.join().map_err(|_| "mock server thread panicked")?;
        Ok(())
    }

    #[test]
    fn update_event_uses_patch() -> Result<()> {
        let (base_url, handle) = start_request_mock_server(
            "PATCH",
            "/calendar/v3/calendars/primary/events/evt-1?sendUpdates=none",
            Some("\"summary\":\"Renamed\""),
            "200 OK",
            r#"{"id":"evt-1","summary":"Renamed"}"#,
        )?;

        let tool =
            GoogleCalendarUpdateEventTool::with_base_url("test-token", "primary", &base_url)?;

        let result = tool.execute(&serde_json::json!({
            "event_id": "evt-1",
            "summary": "Renamed"
        }))?;

        assert!(result.success);
        assert_eq!(result.output["event"]["summary"], "Renamed");
        handle.join().map_err(|_| "mock server thread panicked")?;
        Ok(())
    }

    #[test]
    fn readonly_oauth_credentials_block_mutations_before_network() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let token_path = directory.path().join("token.json");
        let auth = GoogleCalendarAuth::new("client-id", None, &token_path)?;
        auth.store_token(StoredCalendarToken {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: now_unix() + 3600,
            scope: Some(GOOGLE_CALENDAR_EVENTS_READONLY_SCOPE.to_string()),
        })?;

        let tool = GoogleCalendarCreateEventTool::with_auth(auth, "primary")?;
        let result = tool.execute(&serde_json::json!({
            "summary": "Blocked",
            "start": {"dateTime": "2026-09-12T18:00:00+05:30"},
            "end": {"dateTime": "2026-09-12T18:30:00+05:30"}
        }));

        let error = result.expect_err("readonly OAuth credentials should be rejected");
        assert!(error.to_string().contains("event write access"));
        Ok(())
    }

    #[test]
    fn event_write_scope_is_detected() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let token_path = directory.path().join("token.json");
        let auth = GoogleCalendarAuth::new("client-id", None, &token_path)?;

        auth.store_token(StoredCalendarToken {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: now_unix() + 3600,
            scope: Some(GOOGLE_CALENDAR_EVENTS_READONLY_SCOPE.to_string()),
        })?;
        assert!(!auth.has_event_write_scope()?);

        auth.store_token(StoredCalendarToken {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: now_unix() + 3600,
            scope: Some(GOOGLE_CALENDAR_EVENTS_SCOPE.to_string()),
        })?;
        assert!(auth.has_event_write_scope()?);

        auth.store_token(StoredCalendarToken {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: now_unix() + 3600,
            scope: Some("https://www.googleapis.com/auth/calendar".to_string()),
        })?;
        assert!(auth.has_event_write_scope()?);

        Ok(())
    }

    #[test]
    fn oauth_uses_pkce_and_offline_access() -> Result<()> {
        let auth = GoogleCalendarAuth::with_endpoints(
            "client-id",
            None,
            tempfile::tempdir()?.path().join("token.json"),
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
        )?;

        let verifier = random_urlsafe(64)?;
        let challenge = code_challenge(&verifier);
        let state = "state-test";
        let redirect_uri = "http://127.0.0.1:12345/oauth2callback";

        let mut url = auth.auth_endpoint.clone();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("client_id", &auth.client_id);
            query.append_pair("response_type", "code");
            query.append_pair("redirect_uri", redirect_uri);
            query.append_pair("scope", GOOGLE_CALENDAR_EVENTS_SCOPE);
            query.append_pair("access_type", "offline");
            query.append_pair("prompt", "consent");
            query.append_pair("state", state);
            query.append_pair("code_challenge", &challenge);
            query.append_pair("code_challenge_method", "S256");
        }

        let params = url
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("client-id")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(params.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(GOOGLE_CALENDAR_EVENTS_SCOPE)
        );
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some(challenge.as_str())
        );

        Ok(())
    }

    #[test]
    fn oauth_token_state_persists_with_private_permissions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let token_path = directory.path().join("google-calendar-token.json");
        let auth = GoogleCalendarAuth::new("client-id", None, &token_path)?;

        auth.store_token(StoredCalendarToken {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: now_unix() + 3600,
            scope: Some(GOOGLE_CALENDAR_EVENTS_READONLY_SCOPE.to_string()),
        })?;

        let loaded = load_calendar_token(&token_path)?.expect("token should persist");
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&token_path)?.permissions().mode() & 0o777,
                0o600
            );
        }

        Ok(())
    }

    #[test]
    fn oauth_refresh_updates_and_persists_access_token() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("token mock accept failed");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];

            loop {
                let read = stream.read(&mut buffer).expect("token mock read failed");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let header_end = header_end + 4;
                    let header_text = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = header_text
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + content_length {
                        break;
                    }
                }
            }

            let request_text = String::from_utf8_lossy(&request);
            assert!(request_text.starts_with("POST /token"));
            assert!(request_text.contains("grant_type=refresh_token"));
            assert!(request_text.contains("refresh_token=refresh-token"));
            assert!(request_text.contains("client_id=client-id"));

            let body = r#"{
                "access_token": "refreshed-access",
                "expires_in": 3600,
                "scope": "https://www.googleapis.com/auth/calendar.events.readonly"
            }"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("token mock write failed");
        });

        let directory = tempfile::tempdir()?;
        let token_path = directory.path().join("token.json");
        let auth = GoogleCalendarAuth::with_endpoints(
            "client-id",
            Some("client-secret".to_string()),
            &token_path,
            "https://accounts.google.com/o/oauth2/v2/auth",
            &format!("http://{address}/token"),
        )?;

        auth.store_token(StoredCalendarToken {
            access_token: "expired-access".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: 0,
            scope: Some(GOOGLE_CALENDAR_EVENTS_READONLY_SCOPE.to_string()),
        })?;

        let access_token = auth.access_token()?;

        assert_eq!(access_token, "refreshed-access");
        assert_eq!(
            load_calendar_token(&token_path)?.unwrap().access_token,
            "refreshed-access"
        );
        assert_eq!(
            load_calendar_token(&token_path)?
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("refresh-token")
        );

        handle.join().map_err(|_| "token mock thread panicked")?;
        Ok(())
    }

    #[test]
    fn missing_access_token_is_rejected_before_network_call() {
        assert!(GoogleCalendarClient::new("", "primary").is_err());
    }

    #[test]
    fn invalid_list_arguments_are_rejected() -> Result<()> {
        let client = GoogleCalendarClient::new("test-token", "primary")?;
        assert!(
            client
                .list_events(&serde_json::json!({"max_results": 51}))
                .is_err()
        );
        assert!(
            client
                .list_events(&serde_json::json!({"unknown": true}))
                .is_err()
        );
        Ok(())
    }
}
