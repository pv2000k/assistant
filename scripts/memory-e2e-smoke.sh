#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/assistant-memory-e2e.XXXXXX")"
MEMORY_ROOT="$TMP_DIR/memory"
DB_PATH="$TMP_DIR/assistant.db"
WORKER_PID=""

cleanup() {
    if [[ -n "$WORKER_PID" ]]; then
        kill "$WORKER_PID" 2>/dev/null || true
        wait "$WORKER_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

: "${ASSISTANT_QWEN_URL:=http://127.0.0.1:8080}"
: "${ASSISTANT_QWEN_MODEL:=Qwen3.5-4B-Q4_K_M.gguf}"
: "${ASSISTANT_EMBEDDING_URL:=http://127.0.0.1:8081}"
: "${ASSISTANT_EMBEDDING_MODEL:=bge-small-en-v1.5-q8_0.gguf}"

mkdir -p "$MEMORY_ROOT"

: "${E2E_TIMEOUT_SECONDS:=120}"

export ASSISTANT_MEMORY_ROOT="$MEMORY_ROOT"
export ASSISTANT_SQLITE_DB_PATH="$DB_PATH"
export ASSISTANT_QWEN_URL
export ASSISTANT_QWEN_MODEL
export ASSISTANT_EMBEDDING_URL
export ASSISTANT_EMBEDDING_MODEL

cd "$ROOT_DIR"

echo "[1/6] Starting extraction worker..."
cargo run -q -p sqlite-ingest -- --extract-worker >"$TMP_DIR/worker.log" 2>&1 &
WORKER_PID=$!

sleep 1

if ! kill -0 "$WORKER_PID" 2>/dev/null; then
    cat "$TMP_DIR/worker.log"
    echo "Extraction worker failed to start."
    exit 1
fi

echo "[2/6] Recording a test conversation..."
printf '%s\n%s\n' \
    'My current test codename is Bankai.' \
    ':quit' |
    cargo run -q -p runtime --bin assistant >"$TMP_DIR/assistant-record.log" 2>&1

echo "[3/6] Waiting for the proposal..."
PROPOSAL_ID=""
for _ in $(seq 1 "$E2E_TIMEOUT_SECONDS"); do
    PROPOSAL_ID="$(python3 - "$DB_PATH" <<'PY'
import sqlite3, sys
path = sys.argv[1]
conn = sqlite3.connect(path)
row = conn.execute(
    "SELECT id FROM memory_extraction_proposals WHERE status = 'pending' ORDER BY created_at ASC, id ASC LIMIT 1"
).fetchone()
print(row[0] if row else "")
PY
)"
    [[ -n "$PROPOSAL_ID" ]] && break
    sleep 1
done

if [[ -z "$PROPOSAL_ID" ]]; then
    cat "$TMP_DIR/assistant-record.log"
    cat "$TMP_DIR/worker.log"
    echo "No pending memory proposal appeared."
    exit 1
fi

echo "Proposal: $PROPOSAL_ID"

echo "[4/6] Applying the proposal..."
printf ':memory-accept %s\n:quit\n' "$PROPOSAL_ID" |
    cargo run -q -p runtime --bin assistant >"$TMP_DIR/assistant-accept.log" 2>&1

grep -q 'Applied memory proposal.' "$TMP_DIR/assistant-accept.log"

echo "[5/6] Verifying persisted memory..."
python3 - "$MEMORY_ROOT" "$DB_PATH" "$PROPOSAL_ID" <<'PY'
from pathlib import Path
import sqlite3, sys
memory_root = Path(sys.argv[1])
db_path = sys.argv[2]
proposal_id = sys.argv[3]
files = list((memory_root / 'journal' / 'extracted').glob(f'{proposal_id}-*.md'))
if len(files) != 1:
    raise SystemExit(f'Expected one extracted memory note, found {len(files)}')
text = files[0].read_text()
assert 'Bankai' in text
assert 'source_type: "conversation"' in text
conn = sqlite3.connect(db_path)
status = conn.execute(
    'SELECT status FROM memory_extraction_proposals WHERE id = ?', (proposal_id,)
).fetchone()
assert status == ('applied',), status
print(files[0])
PY

echo "[6/6] Verifying retrieval through the real assistant..."
printf '%s\n%s\n' \
    'What is my current test codename?' \
    ':quit' |
    cargo run -q -p runtime --bin assistant >"$TMP_DIR/assistant-retrieve.log" 2>&1

grep -qi 'Bankai' "$TMP_DIR/assistant-retrieve.log"

echo
cat "$TMP_DIR/assistant-retrieve.log"
echo
echo "E2E memory pipeline: PASS"
