#[derive(Debug, Clone, Default)]
pub struct NoteMeta {
    pub id: Option<String>,
    pub title: Option<String>,
    pub note_type: Option<String>,
    pub namespace: Option<String>,
    pub canonical: Option<bool>,
    pub status: Option<String>,
    pub memory_kind: Option<String>,
    pub task_status: Option<String>,
    pub reminder_status: Option<String>,

    pub captured_at: Option<String>,
    pub occurred_at: Option<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub due_at: Option<String>,
    pub updated_at: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct EntityRecord {
    pub name: String,
    pub entity_type: String,
}

#[derive(Debug, Clone, Default)]
pub struct RelationshipRecord {
    pub source: String,
    pub target: String,
    pub relationship: String,
}

#[derive(Debug, Clone, Default)]
pub struct EventRecord {
    pub event_type: String,
    pub occurred_at: Option<String>,
    pub description: Option<String>,
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StateRecord {
    pub entity: String,
    pub state: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NoteRecord {
    pub id: String,
    pub path: String,
    pub title: String,
    pub note_type: String,
    pub namespace: String,
    pub canonical: bool,
    pub status: String,
    pub memory_kind: Option<String>,
    pub task_status: Option<String>,
    pub reminder_status: Option<String>,

    pub checksum: String,

    pub captured_at: Option<String>,
    pub occurred_at: Option<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub due_at: Option<String>,
    pub updated_at: Option<String>,
    pub created_at: Option<String>,

    pub body: String,

    pub entities: Vec<EntityRecord>,
    pub relationships: Vec<RelationshipRecord>,
    pub events: Vec<EventRecord>,
    pub states: Vec<StateRecord>,
}

#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub id: String,
    pub note_id: String,
    pub ordinal: u64,
    pub text: String,
    pub content_hash: String,
    pub source_anchor: String,
    pub source_start_line: usize,
    pub source_end_line: usize,
}

#[derive(Debug, Default)]
pub struct IndexStats {
    pub notes: usize,
    pub chunks: usize,
    pub frontmatter_created: usize,
    pub frontmatter_updated: usize,
}
