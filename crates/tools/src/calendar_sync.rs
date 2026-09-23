use crate::Result;
use crate::calendar::{GoogleCalendarAuth, GoogleCalendarClient};
use serde_json::json;
use std::env;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

const CALENDAR_ID_ENV: &str = "ASSISTANT_GOOGLE_CALENDAR_ID";
const ACCESS_TOKEN_ENV: &str = "ASSISTANT_GOOGLE_CALENDAR_ACCESS_TOKEN";
const DEFAULT_CALENDAR_ID: &str = "primary";
const DEFAULT_DURATION_MINUTES: i64 = 30;

#[derive(Debug, Clone)]
pub(crate) struct ReminderCalendarSync {
    calendar_id: String,
    client: Option<GoogleCalendarClient>,
}

impl ReminderCalendarSync {
    pub(crate) fn from_env() -> Result<Self> {
        let calendar_id = env::var(CALENDAR_ID_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CALENDAR_ID.to_string());

        let client = if let Ok(access_token) = env::var(ACCESS_TOKEN_ENV) {
            if access_token.trim().is_empty() {
                None
            } else {
                Some(GoogleCalendarClient::new(access_token, &calendar_id)?)
            }
        } else {
            let auth = GoogleCalendarAuth::from_env()?;
            if auth.is_authenticated() || auth.has_client_id() {
                Some(GoogleCalendarClient::with_auth(auth, &calendar_id)?)
            } else {
                None
            }
        };

        Ok(Self {
            calendar_id,
            client,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(client: GoogleCalendarClient, calendar_id: &str) -> Self {
        Self {
            calendar_id: calendar_id.to_string(),
            client: Some(client),
        }
    }

    pub(crate) fn calendar_id(&self) -> &str {
        &self.calendar_id
    }

    pub(crate) fn create_event(
        &self,
        calendar_id: &str,
        title: &str,
        body: &str,
        due_at: &str,
    ) -> Result<(String, String)> {
        let client = self.client.as_ref().ok_or(
            "Google Calendar reminder sync is enabled, but Calendar credentials are not configured.",
        )?;
        let end_at = reminder_end_at(due_at)?;
        let mut arguments = json!({
            "calendar_id": calendar_id,
            "summary": title,
            "start": {"dateTime": due_at},
            "end": {"dateTime": end_at},
            "send_updates": "none",
        });
        if !body.trim().is_empty() {
            arguments["description"] = json!(body);
        }

        let output = client.create_event(&arguments)?.output;
        let event_id = output
            .get("event")
            .and_then(|event| event.get("id"))
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or("Google Calendar create event response did not contain an event ID.")?;

        Ok((calendar_id.to_string(), event_id.to_string()))
    }

    pub(crate) fn update_event(
        &self,
        calendar_id: &str,
        event_id: &str,
        title: &str,
        body: &str,
        due_at: &str,
    ) -> Result<()> {
        let client = self.client.as_ref().ok_or(
            "Google Calendar reminder sync is enabled, but Calendar credentials are not configured.",
        )?;
        let end_at = reminder_end_at(due_at)?;
        let mut arguments = json!({
            "calendar_id": calendar_id,
            "event_id": event_id,
            "summary": title,
            "description": body,
            "start": {"dateTime": due_at},
            "end": {"dateTime": end_at},
            "send_updates": "none",
        });
        if body.trim().is_empty() {
            arguments["description"] = json!("");
        }
        client.update_event(&arguments)?;
        Ok(())
    }
}

fn reminder_end_at(due_at: &str) -> Result<String> {
    let start = OffsetDateTime::parse(due_at, &Rfc3339)
        .map_err(|error| format!("Reminder due_at is not a valid RFC3339 timestamp: {error}"))?;
    Ok((start + Duration::minutes(DEFAULT_DURATION_MINUTES)).format(&Rfc3339)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminder_event_uses_thirty_minute_window() -> Result<()> {
        assert_eq!(
            reminder_end_at("2026-09-22T10:00:00Z")?,
            "2026-09-22T10:30:00Z"
        );
        Ok(())
    }
}
