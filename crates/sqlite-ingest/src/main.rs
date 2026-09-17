use embedding::LlamaCppEmbedder;
use indexer::model::IndexStats;
use sqlite_ingest::reminder_scheduler::{DesktopNotifier, IDLE_SLEEP_SECONDS, ReminderScheduler};
use sqlite_ingest::worker::MemoryExtractionWorker;
use sqlite_memory::SqliteMemoryDb;
use std::{env, path::PathBuf, thread, time::Duration};

const EMBEDDING_DIMENSION: usize = 384;

fn db_path() -> PathBuf {
    env::var_os("ASSISTANT_SQLITE_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assistant-data/sqlite-memory.db"))
}

fn embedding_url() -> String {
    env::var("ASSISTANT_EMBEDDING_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string())
}

fn embedding_model() -> String {
    env::var("ASSISTANT_EMBEDDING_MODEL")
        .unwrap_or_else(|_| "bge-small-en-v1.5-q8_0.gguf".to_string())
}

fn print_stats(stats: &IndexStats) {
    println!("Notes indexed:            {}", stats.notes);

    println!("Chunks indexed:           {}", stats.chunks);
}

fn qwen_url() -> String {
    env::var("ASSISTANT_QWEN_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

fn qwen_model() -> String {
    env::var("ASSISTANT_QWEN_MODEL").unwrap_or_else(|_| "Qwen3.5-4B-Q4_K_M.gguf".to_string())
}

fn memory_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(value) = env::var_os("ASSISTANT_MEMORY_ROOT") {
        return Ok(PathBuf::from(value));
    }
    let home = env::var_os("HOME").ok_or("HOME environment variable is not set.")?;
    Ok(PathBuf::from(home).join("assistant-memory"))
}

fn run_reminder_scheduler_once() -> Result<(), Box<dyn std::error::Error>> {
    let root = memory_root()?;
    std::fs::create_dir_all(&root)?;
    let mut scheduler = ReminderScheduler::new(root, db_path(), DesktopNotifier)?;
    scheduler.ensure_scheduled()?;
    let processed = scheduler.run_once()?;
    if processed {
        println!("Reminder scheduler: checked due reminders.");
    } else {
        println!("Reminder scheduler: nothing was ready to check yet.");
    }
    Ok(())
}

fn run_reminder_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let root = memory_root()?;
    std::fs::create_dir_all(&root)?;
    let mut scheduler = ReminderScheduler::new(root, db_path(), DesktopNotifier)?;
    scheduler.ensure_scheduled()?;
    println!("Reminder daemon: watching for due reminders.");
    loop {
        match scheduler.run_once() {
            Ok(true) => continue,
            Ok(false) => thread::sleep(Duration::from_secs(IDLE_SLEEP_SECONDS)),
            Err(error) => {
                eprintln!("Reminder daemon: {}", error);
                thread::sleep(Duration::from_secs(IDLE_SLEEP_SECONDS));
            }
        }
    }
}

fn run_extraction_worker_once() -> Result<(), Box<dyn std::error::Error>> {
    let db = SqliteMemoryDb::open(db_path())?;
    db.initialize_schema()?;
    let models = model_router::ModelRouter::new(qwen_url(), qwen_model());
    MemoryExtractionWorker::new(db, models).run_once()
}

fn run_extraction_worker() -> Result<(), Box<dyn std::error::Error>> {
    let db = SqliteMemoryDb::open(db_path())?;
    db.initialize_schema()?;
    let models = model_router::ModelRouter::new(qwen_url(), qwen_model());
    let worker = MemoryExtractionWorker::new(db, models);
    loop {
        if let Err(error) = worker.run_once() {
            eprintln!("Memory extraction worker: {}", error);
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();

    if arguments
        .iter()
        .any(|argument| argument == "--extract-once")
    {
        return run_extraction_worker_once();
    }

    if arguments
        .iter()
        .any(|argument| argument == "--extract-worker")
    {
        return run_extraction_worker();
    }

    if arguments
        .iter()
        .any(|argument| argument == "--reminder-scheduler-once")
    {
        return run_reminder_scheduler_once();
    }

    if arguments
        .iter()
        .any(|argument| argument == "--reminder-daemon")
    {
        return run_reminder_daemon();
    }

    let snapshot = indexer::build_snapshot()?;

    print_stats(&snapshot.stats);

    let mut db = SqliteMemoryDb::open(db_path())?;

    db.initialize_schema()?;

    db.initialize_vector_index(EMBEDDING_DIMENSION)?;

    db.sync_snapshot(&snapshot)?;
    db.sync_semantics(&snapshot)?;

    let embedder = LlamaCppEmbedder::new(embedding_url(), embedding_model());

    let mut generated = 0usize;
    let mut reused = 0usize;

    for chunk in &snapshot.chunks {
        if db.has_chunk_embedding(&chunk.id)? {
            reused += 1;
            continue;
        }

        let embedding = embedder.embed(&chunk.text)?;

        db.upsert_chunk_embedding(&chunk.id, &embedding)?;

        generated += 1;
    }

    let counts = db.semantic_counts()?;

    println!();
    println!("============================================================");
    println!("SQLite persistence");
    println!("============================================================");
    println!("Notes:                    {}", counts.notes);
    println!("Chunks:                   {}", counts.chunks);
    println!("Entities:                 {}", counts.entities);
    println!("Relationships:            {}", counts.relationships);
    println!("Events:                   {}", counts.events);
    println!("Event/entity links:       {}", counts.event_entities);
    println!("States:                   {}", counts.states);
    println!("Tasks:                    {}", counts.tasks);
    println!("Reminders:                {}", counts.reminders);

    println!();
    println!("============================================================");
    println!("Embeddings");
    println!("============================================================");
    println!("Dimension:                {}", EMBEDDING_DIMENSION);
    println!("Generated:                {}", generated);
    println!("Reused:                   {}", reused);

    if counts.notes != i64::try_from(snapshot.notes.len())? {
        return Err("SQLite note count mismatch.".into());
    }

    if counts.chunks != i64::try_from(snapshot.chunks.len())? {
        return Err("SQLite chunk count mismatch.".into());
    }

    if generated + reused != snapshot.chunks.len() {
        return Err("Not every active chunk has an embedding.".into());
    }

    println!();
    println!("Verification: PASS");

    Ok(())
}
