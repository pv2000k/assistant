use memory::MemoryDb;
use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = env::var_os("ASSISTANT_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assistant-data/memory.lbdb"));

    let db = MemoryDb::open(&db_path)?;

    db.initialize_schema()?;
    db.verify_basic_schema()?;

    println!("Schema version: {}", db.schema_version()?);
    println!("Ladybug Memory Schema v1 ready.");

    Ok(())
}
