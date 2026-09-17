#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/assistant-task-reminder-e2e.XXXXXX")"
MEMORY_ROOT="$TMP_DIR/memory"
DB_PATH="$TMP_DIR/assistant.db"

cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

: "${ASSISTANT_QWEN_URL:=http://127.0.0.1:8080}"
: "${ASSISTANT_QWEN_MODEL:=Qwen3.5-4B-Q4_K_M.gguf}"
: "${ASSISTANT_EMBEDDING_URL:=http://127.0.0.1:8081}"
: "${ASSISTANT_EMBEDDING_MODEL:=bge-small-en-v1.5-q8_0.gguf}"

mkdir -p "$MEMORY_ROOT"

export ASSISTANT_MEMORY_ROOT="$MEMORY_ROOT"
export ASSISTANT_SQLITE_DB_PATH="$DB_PATH"
export ASSISTANT_QWEN_URL
export ASSISTANT_QWEN_MODEL
export ASSISTANT_EMBEDDING_URL
export ASSISTANT_EMBEDDING_MODEL

cd "$ROOT_DIR"

run_assistant() {
    local log_file="$1"
    shift
    if ! printf '%s\n' "$@" | cargo run -q -p runtime --bin assistant >"$log_file" 2>&1; then
        echo "assistant exited with a non-zero status; log follows:" >&2
        cat "$log_file" >&2
        return 1
    fi
}

require_log() {
    local log_file="$1"
    local pattern="$2"
    if ! grep -qi "$pattern" "$log_file"; then
        echo "Expected '$pattern' in $log_file; log follows:" >&2
        cat "$log_file" >&2
        return 1
    fi
}

echo "[1/10] Creating a task through the real assistant..."
run_assistant "$TMP_DIR/task-create.log" \
    'I have a task for tomorrow. Complete the AI assistant project.' \
    'y' \
    ':quit'
require_log "$TMP_DIR/task-create.log" 'Complete the AI assistant project'
require_log "$TMP_DIR/task-create.log" 'Approved\.'

echo "[2/10] Verifying task Markdown and SQLite persistence..."
python3 - "$MEMORY_ROOT" "$DB_PATH" <<'PY'
from datetime import datetime, timedelta
from pathlib import Path
import sqlite3, sys

memory_root = Path(sys.argv[1])
db_path = sys.argv[2]
conn = sqlite3.connect(db_path)
row = conn.execute(
    "SELECT id, note_id, title, status, due_at FROM tasks ORDER BY created_at, id LIMIT 1"
).fetchone()
assert row is not None, "created task missing from SQLite"
id, note_id, title, status, due_at = row
assert title == "Complete the AI assistant project", row
assert status == "open", row
assert id == f"task:{note_id}", row
assert due_at is not None, row
stored = datetime.fromisoformat(due_at.replace("Z", "+00:00"))
assert stored.astimezone().date() == (datetime.now().astimezone() + timedelta(days=1)).date()
note_path = conn.execute("SELECT path FROM notes WHERE id = ?", (note_id,)).fetchone()
assert note_path is not None, note_id
note = memory_root / note_path[0]
assert note.is_file(), note
text = note.read_text()
assert 'memory_kind: "task"' in text
assert 'task_status: "open"' in text
assert 'title: "Complete the AI assistant project"' in text
assert f'due_at: "{due_at}"' in text
print(id)
print(note)
print(due_at)
PY

echo "[3/10] Retrieving tasks due tomorrow..."
run_assistant "$TMP_DIR/task-list.log" \
    'What tasks are due tomorrow?' \
    ':quit'
require_log "$TMP_DIR/task-list.log" 'Complete the AI assistant project'

echo "[4/10] Completing the task by natural-language identity..."
run_assistant "$TMP_DIR/task-complete.log" \
    'Mark the task called Complete the AI assistant project as complete.' \
    'y' \
    ':quit'
require_log "$TMP_DIR/task-complete.log" 'Approved\.'

echo "[5/10] Verifying completed task and default open-task view..."
python3 - "$MEMORY_ROOT" "$DB_PATH" <<'PY'
from pathlib import Path
import sqlite3, sys

memory_root = Path(sys.argv[1])
conn = sqlite3.connect(sys.argv[2])
row = conn.execute(
    "SELECT note_id, title, status FROM tasks WHERE title = ?", ("Complete the AI assistant project",)
).fetchone()
assert row is not None and row[2] == "completed", row
note_path = conn.execute("SELECT path FROM notes WHERE id = ?", (row[0],)).fetchone()
assert note_path is not None
text = (memory_root / note_path[0]).read_text()
assert 'task_status: "completed"' in text
PY
run_assistant "$TMP_DIR/task-open.log" \
    'Show my open tasks.' \
    ':quit'
if grep -qi 'Review RCM report' "$TMP_DIR/task-open.log"; then
    echo "Completed task incorrectly appeared in the default open-task view."
    cat "$TMP_DIR/task-open.log"
    exit 1
fi

echo "[6/10] Creating a reminder through the real assistant..."
run_assistant "$TMP_DIR/reminder-create.log" \
    'Remind me to submit the application tomorrow at 5 PM.' \
    'y' \
    ':quit'
require_log "$TMP_DIR/reminder-create.log" 'submit the application'
require_log "$TMP_DIR/reminder-create.log" 'Approved\.'

echo "[7/10] Verifying reminder Markdown and SQLite persistence..."
python3 - "$MEMORY_ROOT" "$DB_PATH" <<'PY'
from datetime import datetime, timedelta
from pathlib import Path
import sqlite3, sys

memory_root = Path(sys.argv[1])
conn = sqlite3.connect(sys.argv[2])
row = conn.execute(
    "SELECT id, note_id, title, status, due_at FROM reminders ORDER BY created_at, id LIMIT 1"
).fetchone()
assert row is not None, "created reminder missing from SQLite"
id, note_id, title, status, due_at = row
assert "submit the application" in title.lower(), row
assert status == "scheduled", row
assert id == f"reminder:{note_id}", row
assert due_at is not None, row
stored = datetime.fromisoformat(due_at.replace("Z", "+00:00"))
assert stored.astimezone().date() == (datetime.now().astimezone() + timedelta(days=1)).date()
note_path = conn.execute("SELECT path FROM notes WHERE id = ?", (note_id,)).fetchone()
assert note_path is not None
note = memory_root / note_path[0]
assert note.is_file()
text = note.read_text()
assert 'memory_kind: "reminder"' in text
assert 'reminder_status: "scheduled"' in text
PY

echo "[8/10] Retrieving reminders due tomorrow..."
run_assistant "$TMP_DIR/reminder-list.log" \
    'What reminders do I have tomorrow?' \
    ':quit'
require_log "$TMP_DIR/reminder-list.log" 'submit the application'

echo "[9/10] Completing the reminder by natural-language identity..."
run_assistant "$TMP_DIR/reminder-complete.log" \
    'Mark the reminder about submitting the application as complete.' \
    'y' \
    ':quit'
require_log "$TMP_DIR/reminder-complete.log" 'Approved\.'

echo "[10/10] Verifying completed reminder is excluded from the default scheduled view..."
python3 - "$MEMORY_ROOT" "$DB_PATH" <<'PY'
from pathlib import Path
import sqlite3, sys

memory_root = Path(sys.argv[1])
conn = sqlite3.connect(sys.argv[2])
row = conn.execute(
    "SELECT note_id, title, status FROM reminders WHERE lower(title) LIKE '%submit the application%'"
).fetchone()
assert row is not None and row[2] == "completed", row
note_path = conn.execute("SELECT path FROM notes WHERE id = ?", (row[0],)).fetchone()
assert note_path is not None
text = (memory_root / note_path[0]).read_text()
assert 'reminder_status: "completed"' in text
PY
run_assistant "$TMP_DIR/reminder-scheduled.log" \
    'Show my scheduled reminders.' \
    ':quit'
if grep -qi 'submit the application' "$TMP_DIR/reminder-scheduled.log"; then
    echo "Completed reminder incorrectly appeared in the default scheduled view."
    cat "$TMP_DIR/reminder-scheduled.log"
    exit 1
fi

echo
echo "Task/reminder operational E2E: PASS"
