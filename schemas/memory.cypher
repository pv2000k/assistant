CREATE NODE TABLE IF NOT EXISTS SystemMeta (
    key STRING PRIMARY KEY,
    value STRING,
    updated_at TIMESTAMP
);

CREATE NODE TABLE IF NOT EXISTS Note (
    id STRING PRIMARY KEY,
    path STRING,
    title STRING,
    note_type STRING,
    namespace_type STRING,
    canonical BOOLEAN DEFAULT false,
    status STRING,
    checksum STRING,
    created_at TIMESTAMP,
    updated_at TIMESTAMP
);

CREATE NODE TABLE IF NOT EXISTS Entity (
    id STRING PRIMARY KEY,
    name STRING,
    entity_type STRING,
    domain STRING,
    aliases STRING[],
    status STRING,
    created_at TIMESTAMP,
    updated_at TIMESTAMP
);

CREATE NODE TABLE IF NOT EXISTS Memory (
    id STRING PRIMARY KEY,
    content STRING,
    memory_type STRING,

    captured_at TIMESTAMP,
    occurred_at TIMESTAMP,
    valid_from TIMESTAMP,
    valid_to TIMESTAMP,
    due_at TIMESTAMP,
    updated_at TIMESTAMP,
    verified_at TIMESTAMP,

    confidence DOUBLE,
    status STRING,

    revision_group_id STRING,
    version_number INT32,
    content_hash STRING,

    source_type STRING,
    source_id STRING,
    source_path STRING,
    source_anchor STRING,
    source_excerpt STRING,

    created_at TIMESTAMP
);

CREATE NODE TABLE IF NOT EXISTS State (
    id STRING PRIMARY KEY,
    content STRING,
    valid_from TIMESTAMP,
    valid_to TIMESTAMP,
    confidence DOUBLE,
    status STRING,

    source_type STRING,
    source_id STRING,
    source_path STRING,
    source_anchor STRING,
    source_excerpt STRING,

    created_at TIMESTAMP,
    updated_at TIMESTAMP
);

CREATE NODE TABLE IF NOT EXISTS Task (
    id STRING PRIMARY KEY,
    title STRING,
    description STRING,
    task_status STRING,
    priority INT32,

    created_at TIMESTAMP,
    due_at TIMESTAMP,
    completed_at TIMESTAMP,
    updated_at TIMESTAMP,

    confidence DOUBLE,

    source_type STRING,
    source_id STRING,
    source_path STRING,
    source_anchor STRING,
    source_excerpt STRING
);

CREATE NODE TABLE IF NOT EXISTS Reminder (
    id STRING PRIMARY KEY,
    title STRING,
    scheduled_at TIMESTAMP,
    reminder_status STRING,
    channel STRING,
    recurrence STRING,

    created_at TIMESTAMP,
    updated_at TIMESTAMP,

    source_type STRING,
    source_id STRING,
    source_path STRING
);

CREATE NODE TABLE IF NOT EXISTS Resource (
    id STRING PRIMARY KEY,
    title STRING,
    resource_type STRING,
    url STRING,
    path STRING,
    description STRING,

    captured_at TIMESTAMP,
    updated_at TIMESTAMP,
    checksum STRING,
    status STRING
);

CREATE NODE TABLE IF NOT EXISTS Attachment (
    id STRING PRIMARY KEY,
    relative_path STRING,
    filename STRING,
    mime_type STRING,
    size_bytes INT64,
    checksum STRING,
    storage_status STRING,

    created_at TIMESTAMP,
    updated_at TIMESTAMP
);

CREATE NODE TABLE IF NOT EXISTS Chunk (
    id STRING PRIMARY KEY,
    text STRING,
    ordinal INT64,
    token_count INT64,

    created_at TIMESTAMP,
    updated_at TIMESTAMP
);

CREATE NODE TABLE IF NOT EXISTS Embedding (
    id STRING PRIMARY KEY,
    model_id STRING,
    dimensions INT32,
    vector FLOAT[],

    embedding_status STRING,
    error STRING,

    created_at TIMESTAMP,
    updated_at TIMESTAMP
);

CREATE NODE TABLE IF NOT EXISTS Job (
    id STRING PRIMARY KEY,
    job_type STRING,
    target_type STRING,
    target_id STRING,

    job_status STRING,
    priority INT32,
    attempts INT32,
    next_run_at TIMESTAMP,

    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_error STRING
);

CREATE REL TABLE IF NOT EXISTS RECORDED_IN (
    FROM Memory TO Note,
    created_at TIMESTAMP,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS ABOUT (
    FROM Memory TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS MEMORY_RELATED_TO (
    FROM Memory TO Memory,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS SUPERSEDES (
    FROM Memory TO Memory,
    created_at TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS HAS_CANONICAL_NOTE (
    FROM Entity TO Note,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS RELATED_TO (
    FROM Entity TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS PART_OF (
    FROM Entity TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS USES (
    FROM Entity TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS RUNS_ON (
    FROM Entity TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS WORKS_ON (
    FROM Entity TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS CANDIDATE_FOR (
    FROM Entity TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS HAS_STATE (
    FROM Entity TO State,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS STATE_EVOLVES_INTO (
    FROM State TO State,
    created_at TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS REPRESENTS_STATE (
    FROM Memory TO State,
    created_at TIMESTAMP,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS REPRESENTS_TASK (
    FROM Memory TO Task,
    created_at TIMESTAMP,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS TASK_ABOUT (
    FROM Task TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS HAS_REMINDER (
    FROM Task TO Reminder,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS RESOURCE_RELATED_TO (
    FROM Resource TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    last_verified TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS RESOURCE_SUPPORTS (
    FROM Resource TO Memory,
    created_at TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS ATTACHED_TO (
    FROM Attachment TO Note,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS ATTACHMENT_RELATED_TO (
    FROM Attachment TO Entity,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS ATTACHMENT_SUPPORTS (
    FROM Attachment TO Memory,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    source_type STRING,
    source_id STRING,
    source_path STRING,
    confidence DOUBLE,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS ATTACHMENT_REPRESENTS (
    FROM Attachment TO Resource,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS PART_OF_CHUNK (
    FROM Chunk TO Note,
    created_at TIMESTAMP,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS HAS_EMBEDDING (
    FROM Memory TO Embedding,
    created_at TIMESTAMP,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS CHUNK_HAS_EMBEDDING (
    FROM Chunk TO Embedding,
    created_at TIMESTAMP,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS NOTE_HAS_EMBEDDING (
    FROM Note TO Embedding,
    created_at TIMESTAMP,
    status STRING
);

CREATE REL TABLE IF NOT EXISTS RESOURCE_HAS_EMBEDDING (
    FROM Resource TO Embedding,
    created_at TIMESTAMP,
    status STRING
);
