//! Reminder scheduling and triggering.
//!
//! This module turns `scheduled` reminders whose `due_at` has passed into
//! `triggered` reminders, using the existing Markdown-authoritative mutation
//! path (`tools::ReminderMutationTool`), then delivers a best-effort desktop
//! notification. Scheduling itself reuses the existing generic job queue
//! (`check_reminders` job type) rather than a bespoke timer loop: a
//! `check_reminders` job re-enqueues itself `POLL_INTERVAL_SECONDS` in the
//! future every time it runs, so `claim_next_job_of_type` naturally paces
//! execution without the daemon needing to track wall-clock scheduling
//! itself.
//!
//! Delivery is intentionally decoupled from the triggering logic behind the
//! `Notifier` trait so the core scheduling behavior can be tested without a
//! desktop session.

use serde_json::json;
use sqlite_memory::{ReminderRecord, SqliteMemoryDb};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tools::{Tool, tasks::ReminderMutationTool};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Job type used for the recurring "check due reminders" job.
pub const CHECK_REMINDERS_JOB: &str = "check_reminders";

/// How often the check_reminders job re-schedules itself after running.
pub const POLL_INTERVAL_SECONDS: i64 = 30;

/// How long the daemon sleeps between claim attempts when no job is ready.
pub const IDLE_SLEEP_SECONDS: u64 = 5;

/// Delivers a reminder notification to the user. Kept as a trait so the
/// scheduling/triggering logic can be tested without a real desktop
/// notification server.
pub trait Notifier {
    fn notify(&self, title: &str, body: &str);
}

/// Delivers notifications via `notify-send` (D-Bus/libnotify), matching the
/// project's existing "small Rust component + standard Linux desktop
/// facility" preference over a bespoke notification stack. Failures (for
/// example, `notify-send` not being installed, or no desktop session being
/// available) are logged and otherwise ignored: a missing notification must
/// never prevent the reminder from being marked triggered.
pub struct DesktopNotifier;

impl Notifier for DesktopNotifier {
    fn notify(&self, title: &str, body: &str) {
        let result = Command::new("notify-send")
            .arg("--app-name=Personal Assistant")
            .arg(title)
            .arg(body)
            .output();

        match result {
            Ok(output) if !output.status.success() => {
                eprintln!(
                    "Reminder scheduler: notify-send exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(error) => {
                eprintln!(
                    "Reminder scheduler: could not run notify-send ({error}). \
                     Is libnotify installed and is a desktop session available?"
                );
            }
            Ok(_) => {}
        }
    }
}

/// Returns the subset of `reminders` that are still scheduled and whose
/// `due_at` is at or before `now`. Reminders with no `due_at`, or with a due
/// date that fails to parse, are left alone rather than triggered or
/// dropped silently; a malformed due date is a data problem, not something
/// the scheduler should paper over by firing anyway.
fn due_reminders<'a>(
    reminders: &'a [ReminderRecord],
    now: OffsetDateTime,
) -> Vec<&'a ReminderRecord> {
    reminders
        .iter()
        .filter(|reminder| reminder.status == "scheduled")
        .filter(|reminder| {
            reminder
                .due_at
                .as_deref()
                .and_then(|due| OffsetDateTime::parse(due, &Rfc3339).ok())
                .is_some_and(|due| due <= now)
        })
        .collect()
}

pub struct ReminderScheduler<N: Notifier> {
    root: PathBuf,
    db: SqliteMemoryDb,
    reminder_tool: ReminderMutationTool,
    notifier: N,
}

impl<N: Notifier> ReminderScheduler<N> {
    pub fn new(root: impl AsRef<Path>, db_path: impl AsRef<Path>, notifier: N) -> Result<Self> {
        let db = SqliteMemoryDb::open(db_path.as_ref())?;
        db.initialize_schema()?;
        let reminder_tool = ReminderMutationTool::new(root.as_ref(), db_path.as_ref())?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            db,
            reminder_tool,
            notifier,
        })
    }

    /// Ensures a `check_reminders` job exists so the daemon has something to
    /// claim. Safe to call on every startup: it does nothing if a
    /// queued/running/failed job of this type already exists, so restarting
    /// the daemon never stacks up duplicate recurring jobs.
    pub fn ensure_scheduled(&self) -> Result<()> {
        if self.db.has_pending_job_of_type(CHECK_REMINDERS_JOB)? {
            return Ok(());
        }
        let now = now_rfc3339()?;
        self.db
            .enqueue_job(CHECK_REMINDERS_JOB, None, None, 0, Some(now.as_str()))?;
        Ok(())
    }

    /// Finds reminders that are due, triggers each one through the
    /// Markdown-authoritative mutation path, and notifies for each success.
    /// A failure to trigger one reminder (for example, a missing source
    /// note) is logged and does not stop the remaining reminders in the
    /// batch from being processed.
    ///
    /// `ReminderMutationTool::execute` only writes Markdown — in the normal
    /// conversational path, `IndexedToolExecutor` is what reconciles SQLite
    /// afterward. The scheduler calls the tool directly (there is no
    /// conversation turn here), so it takes on that reconciliation itself:
    /// once per batch, not per reminder, and without touching embeddings.
    /// Triggering only ever changes `reminder_status` in frontmatter, never
    /// note body/chunk text, so existing chunk embeddings stay valid and
    /// there is no need to involve the embedding server just to flip a
    /// status flag.
    pub fn check_and_trigger_due_reminders(&mut self) -> Result<usize> {
        let now = OffsetDateTime::now_utc();
        // A generous limit: this is a personal second brain, not a
        // multi-tenant system, and the query is cheap. Revisit if reminder
        // volume ever makes this a real bottleneck.
        let reminders = self.db.reminders(Some("scheduled"), 500)?;
        let due = due_reminders(&reminders, now);

        let mut triggered = 0usize;
        for reminder in due {
            match self.reminder_tool.execute(&json!({
                "operation": "trigger",
                "id": reminder.id,
            })) {
                Ok(result) if result.success => {
                    let due_local = result
                        .output
                        .get("due_at_local")
                        .and_then(|value| value.as_str())
                        .unwrap_or("now");
                    self.notifier
                        .notify(&reminder.title, &format!("Due {due_local}"));
                    triggered += 1;
                }
                Ok(result) => {
                    eprintln!(
                        "Reminder scheduler: failed to trigger reminder {}: {:?}",
                        reminder.id, result.output
                    );
                }
                Err(error) => {
                    eprintln!(
                        "Reminder scheduler: failed to trigger reminder {}: {error}",
                        reminder.id
                    );
                }
            }
        }

        if triggered > 0 {
            let snapshot = indexer::build_snapshot_from(&self.root)?;
            self.db.sync_snapshot(&snapshot)?;
            self.db.sync_semantics(&snapshot)?;
        }

        Ok(triggered)
    }

    /// Claims the next `check_reminders` job if one is ready, processes due
    /// reminders, completes the job, and re-enqueues the next check
    /// `POLL_INTERVAL_SECONDS` in the future. Returns `true` if a job was
    /// claimed and processed, `false` if there was nothing ready to claim.
    ///
    /// Infrastructure failures (the claim/complete/enqueue calls themselves
    /// failing) fail the job for retry rather than losing it silently.
    /// Failures triggering individual reminders are handled inside
    /// `check_and_trigger_due_reminders` and do not fail the job.
    pub fn run_once(&mut self) -> Result<bool> {
        let now = now_rfc3339()?;
        let Some(job) = self.db.claim_next_job_of_type(&now, CHECK_REMINDERS_JOB)? else {
            return Ok(false);
        };

        match self.check_and_trigger_due_reminders() {
            Ok(_) => {
                self.db.complete_job(&job.id)?;
                let next_run_at = reschedule_timestamp()?;
                self.db.enqueue_job(
                    CHECK_REMINDERS_JOB,
                    None,
                    None,
                    0,
                    Some(next_run_at.as_str()),
                )?;
            }
            Err(error) => {
                let retry_at = reschedule_timestamp()?;
                self.db.fail_job(&job.id, &error.to_string(), &retry_at)?;
            }
        }

        Ok(true)
    }
}

fn now_rfc3339() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

fn reschedule_timestamp() -> Result<String> {
    Ok(
        (OffsetDateTime::now_utc() + time::Duration::seconds(POLL_INTERVAL_SECONDS))
            .format(&Rfc3339)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    struct RecordingNotifier {
        calls: RefCell<Vec<(String, String)>>,
    }

    impl RecordingNotifier {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Notifier for RecordingNotifier {
        fn notify(&self, title: &str, body: &str) {
            self.calls
                .borrow_mut()
                .push((title.to_string(), body.to_string()));
        }
    }

    fn record(title: &str, due_at: &str, status: &str) -> ReminderRecord {
        ReminderRecord {
            id: format!("reminder:{title}"),
            note_id: format!("note:{title}"),
            title: title.to_string(),
            body: String::new(),
            status: status.to_string(),
            due_at: Some(due_at.to_string()),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn due_reminders_filters_status_and_time() {
        let now = OffsetDateTime::parse("2026-09-14T10:00:00Z", &Rfc3339).unwrap();
        let reminders = vec![
            record("Past due, scheduled", "2026-09-14T09:00:00Z", "scheduled"),
            record("Future, scheduled", "2026-09-14T11:00:00Z", "scheduled"),
            record(
                "Past due, already triggered",
                "2026-09-14T09:00:00Z",
                "triggered",
            ),
            record("Exactly now", "2026-09-14T10:00:00Z", "scheduled"),
        ];

        let due = due_reminders(&reminders, now);
        let titles: Vec<&str> = due.iter().map(|reminder| reminder.title.as_str()).collect();

        assert_eq!(titles, vec!["Past due, scheduled", "Exactly now"]);
    }

    #[test]
    fn due_reminders_ignores_missing_or_malformed_due_dates() {
        let now = OffsetDateTime::parse("2026-09-14T10:00:00Z", &Rfc3339).unwrap();
        let mut no_due = record("No due date", "2026-09-14T09:00:00Z", "scheduled");
        no_due.due_at = None;
        let malformed = ReminderRecord {
            due_at: Some("not-a-date".to_string()),
            ..record("Malformed due", "2026-09-14T09:00:00Z", "scheduled")
        };

        let items = [no_due, malformed];
        let due = due_reminders(&items, now);
        assert!(due.is_empty());
    }

    /// Creates a reminder through the real `ReminderMutationTool` create
    /// path (rather than hand-writing frontmatter) so the fixture is
    /// guaranteed to match whatever shape the mutation tool actually
    /// produces, then indexes it.
    fn seed_reminder(root: &Path, db_path: &Path, title: &str, due_at: &str) -> Result<()> {
        let tool = ReminderMutationTool::new(root, db_path)?;
        let created = tool.execute(&json!({
            "operation": "create",
            "title": title,
            "due": due_at,
        }))?;
        assert!(created.success, "seed reminder creation failed");
        Ok(())
    }

    fn refresh(root: &Path, db: &mut SqliteMemoryDb) -> Result<()> {
        let snapshot = indexer::build_snapshot_from(root)?;
        db.sync_snapshot(&snapshot)?;
        db.sync_semantics(&snapshot)?;
        Ok(())
    }

    #[test]
    fn check_and_trigger_flips_status_and_notifies() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("memory");
        fs::create_dir_all(&root)?;
        let db_path = temp.path().join("assistant.db");

        let mut db = SqliteMemoryDb::open(&db_path)?;
        db.initialize_schema()?;
        seed_reminder(&root, &db_path, "Call the dentist", "2020-01-01T09:00:00Z")?;
        refresh(&root, &mut db)?;
        assert_eq!(db.reminders(Some("scheduled"), 10)?.len(), 1);

        let mut scheduler = ReminderScheduler::new(&root, &db_path, RecordingNotifier::new())?;
        let triggered = scheduler.check_and_trigger_due_reminders()?;
        assert_eq!(triggered, 1);
        assert_eq!(scheduler.notifier.calls.borrow().len(), 1);
        assert_eq!(scheduler.notifier.calls.borrow()[0].0, "Call the dentist");

        // Re-read through a fresh handle to confirm the change is really
        // persisted (Markdown -> reindex), not just visible on the
        // connection that made it.
        let verify = SqliteMemoryDb::open(&db_path)?;
        assert_eq!(verify.reminders(Some("scheduled"), 10)?.len(), 0);
        assert_eq!(verify.reminders(Some("triggered"), 10)?.len(), 1);

        Ok(())
    }

    #[test]
    fn check_and_trigger_is_a_no_op_when_nothing_is_due() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("memory");
        fs::create_dir_all(&root)?;
        let db_path = temp.path().join("assistant.db");

        let mut db = SqliteMemoryDb::open(&db_path)?;
        db.initialize_schema()?;
        seed_reminder(&root, &db_path, "Future reminder", "2099-01-01T09:00:00Z")?;
        refresh(&root, &mut db)?;

        let mut scheduler = ReminderScheduler::new(&root, &db_path, RecordingNotifier::new())?;
        let triggered = scheduler.check_and_trigger_due_reminders()?;
        assert_eq!(triggered, 0);
        assert!(scheduler.notifier.calls.borrow().is_empty());
        assert_eq!(
            SqliteMemoryDb::open(&db_path)?
                .reminders(Some("scheduled"), 10)?
                .len(),
            1
        );

        Ok(())
    }

    #[test]
    fn ensure_scheduled_is_idempotent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("memory");
        fs::create_dir_all(&root)?;
        let db_path = temp.path().join("assistant.db");
        let db = SqliteMemoryDb::open(&db_path)?;
        db.initialize_schema()?;

        let scheduler = ReminderScheduler::new(&root, &db_path, RecordingNotifier::new())?;
        scheduler.ensure_scheduled()?;
        scheduler.ensure_scheduled()?;
        scheduler.ensure_scheduled()?;

        assert!(db.has_pending_job_of_type(CHECK_REMINDERS_JOB)?);
        // Only one job should have been created despite three calls.
        let claimed_first = db.claim_next_job_of_type(&now_rfc3339()?, CHECK_REMINDERS_JOB)?;
        assert!(claimed_first.is_some());
        db.complete_job(&claimed_first.unwrap().id)?;
        assert!(
            db.claim_next_job_of_type(&now_rfc3339()?, CHECK_REMINDERS_JOB)?
                .is_none()
        );

        Ok(())
    }

    #[test]
    fn run_once_processes_due_reminder_and_reschedules() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("memory");
        fs::create_dir_all(&root)?;
        let db_path = temp.path().join("assistant.db");
        let mut db = SqliteMemoryDb::open(&db_path)?;
        db.initialize_schema()?;
        seed_reminder(&root, &db_path, "Call the dentist", "2020-01-01T09:00:00Z")?;
        refresh(&root, &mut db)?;

        let mut scheduler = ReminderScheduler::new(&root, &db_path, RecordingNotifier::new())?;
        scheduler.ensure_scheduled()?;

        let processed = scheduler.run_once()?;
        assert!(processed);
        assert_eq!(scheduler.notifier.calls.borrow().len(), 1);

        // A follow-up job should now be scheduled ~POLL_INTERVAL_SECONDS out,
        // so calling run_once immediately again finds nothing claimable yet.
        assert!(!scheduler.run_once()?);

        // Calling with no ready job at all returns false rather than erroring.
        let empty_db_path = temp.path().join("empty.db");
        let empty_db = SqliteMemoryDb::open(&empty_db_path)?;
        empty_db.initialize_schema()?;
        let empty_root = temp.path().join("empty-memory");
        fs::create_dir_all(&empty_root)?;
        let mut empty_scheduler =
            ReminderScheduler::new(&empty_root, &empty_db_path, RecordingNotifier::new())?;
        assert!(!empty_scheduler.run_once()?);

        Ok(())
    }
}
