# Google Calendar integration

Zaraki already contains the Google Calendar backend integration. It uses OAuth 2.0 with PKCE, a loopback browser callback, persistent refresh tokens, and the Google Calendar v3 API.

## What is already implemented

The runtime can register these tools when Calendar is configured:

- `calendar.list_events`
- `calendar.get_event`
- `calendar.create_event`
- `calendar.update_event`

Read operations are available without mutation approval. Event creation and update require the existing tool approval path. Zaraki never exposes or executes Google Calendar event deletion.

OAuth tokens are stored outside the repository by default at:

```text
~/.config/assistant/google-calendar-token.json
```

The token file is written with Unix mode `0600`.

## Google Cloud setup

1. Create or select a Google Cloud project.
2. Enable the **Google Calendar API** for that project.
3. Configure the Google Auth Platform consent screen.
4. Create an OAuth client with application type **Desktop app**.
5. Keep the generated client ID out of source code. The current runtime reads it from `ASSISTANT_GOOGLE_CLIENT_ID`.
6. If Google provides a client secret for the desktop credential, keep it out of source control and provide it through `ASSISTANT_GOOGLE_CLIENT_SECRET`.
7. Start Zaraki with the variables configured.
8. Run the existing runtime command:

```text
:calendar-login
```

Zaraki opens the Google authorization page, performs the PKCE flow through a localhost callback, exchanges the authorization code, and stores the refresh token locally.

Example local environment configuration:

```bash
export ASSISTANT_GOOGLE_CLIENT_ID="your-client-id"
export ASSISTANT_GOOGLE_CLIENT_SECRET="your-client-secret"
export ASSISTANT_GOOGLE_CALENDAR_ID="primary"
```

`ASSISTANT_GOOGLE_CLIENT_SECRET` is optional in the current implementation. Do not put real credentials in this file or in the repository.

## Scopes

The current OAuth flow requests:

```text
https://www.googleapis.com/auth/calendar.events
```

This is the scope used for event creation and update. Google may technically permit deletion under this scope, but Zaraki intentionally does not expose a delete operation. The implementation also tracks whether a stored token has write access and blocks mutations when only the read-only scope is present.

Use the narrowest scope necessary when adding future Calendar capabilities.

## Static access token fallback

The runtime also supports:

```bash
export ASSISTANT_GOOGLE_CALENDAR_ACCESS_TOKEN="..."
```

This is intended for temporary/testing use. It overrides the OAuth path. Do not commit the token.

## Reminder integration

Zaraki reminders remain the source of truth. Google Calendar is an optional projection.

Reminder/calendar sync is explicitly opt-in per reminder. The Reminder form exposes a `Calendar Sync` field with `yes`/`no`; `no` is the default for new reminders and editing a reminder shows its current choice.

With `Calendar Sync = yes`:

```text
Reminder create
  -> Markdown reminder remains authoritative
  -> Google Calendar event is created when a due time exists
  -> Google event ID and sync state are persisted in reminder frontmatter
```

For reminders that are opted into sync:

- updating title/body/due time updates the linked Calendar event
- clearing the due time preserves the linked Calendar event; Zaraki records the sync as `preserved`
- cancelling the reminder preserves the linked Calendar event; Zaraki records the sync as `preserved`
- synchronization failures do not roll back the reminder; the failure is persisted as `calendar_sync_status: error` with `calendar_sync_error`
- a later reminder mutation can retry the synchronization path
- Zaraki never deletes Google Calendar events; deletion is local-only within Zaraki

The persisted reminder metadata is:

- `calendar_sync_enabled`
- `google_calendar_id`
- `google_event_id`
- `calendar_sync_status`
- `calendar_last_synced_at`
- `calendar_sync_error`

A reminder without `calendar_sync_enabled: true` continues to behave locally only. Existing reminders are not silently opted in.

Calendar events created by reminder sync use a 30-minute window starting at the reminder due time and set `sendUpdates=none`; the reminder body is used as the event description when non-empty.

## Security rules

Never commit:

- OAuth client secrets
- downloaded Google credential JSON files
- access tokens
- refresh tokens
- `.env` files containing credentials

The repository `.gitignore` explicitly covers these local credential artifacts. OAuth tokens are stored outside the repository by default.
