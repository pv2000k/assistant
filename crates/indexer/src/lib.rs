pub mod chunker;
pub mod frontmatter;
pub mod hashing;
pub mod indexes;
pub mod model;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::frontmatter::{add_missing_frontmatter, parse};
use crate::hashing::sha256_hex;
use crate::model::{IndexStats, NoteRecord};

fn now_utc() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("failed to format current timestamp")
}

fn infer_namespace(path: &str) -> &'static str {
    if path.starts_with("projects/") {
        "project"
    } else if path.starts_with("topics/") {
        "topic"
    } else if path.starts_with("people/") {
        "person"
    } else if path == "ASSISTANT.md" || path == "ENVIRONMENT.md" {
        "system"
    } else {
        "global"
    }
}

fn infer_note_type(path: &str) -> &'static str {
    if path == "ASSISTANT.md" || path == "ENVIRONMENT.md" {
        "instruction"
    } else if path == "TIMELINE.md" {
        "timeline"
    } else if path == "INDEX.md" || path.ends_with("/INDEX.md") {
        "index"
    } else if path == "backlog.md" {
        "backlog"
    } else if path.starts_with("journal/") {
        "journal"
    } else if path.starts_with("research/") {
        "research"
    } else if path.starts_with("resources/") {
        "reference"
    } else if path.starts_with("projects/")
        || path.starts_with("topics/")
        || path.starts_with("people/")
    {
        "canonical"
    } else {
        "other"
    }
}

fn infer_title(body: &str, path: &str) -> String {
    for line in body.lines() {
        let trimmed = line.trim();

        if let Some(title) = trimmed.strip_prefix("# ")
            && !title.trim().is_empty()
        {
            return title.trim().to_string();
        }
    }

    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(path)
        .replace(['_', '-'], " ")
}

fn is_generated_note(path: &str) -> bool {
    path == "INDEX.md" || path == "TIMELINE.md" || path.ends_with("/INDEX.md")
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("discovered file lies outside memory root")
        .to_string_lossy()
        .replace('\\', "/")
}

fn should_skip_directory(name: &str) -> bool {
    matches!(name, ".git" | ".cache" | "archive" | "processing")
}

fn discover(current: &Path, results: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");

            if should_skip_directory(name) {
                continue;
            }

            discover(&path, results)?;
            continue;
        }

        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }

        results.push(path);
    }

    Ok(())
}

fn enforce_system_canonical(path: &str, content: &str) -> (String, bool) {
    if !matches!(path, "ASSISTANT.md" | "ENVIRONMENT.md") {
        return (content.to_string(), false);
    }

    let mut lines = content.lines();
    let Some(first_line) = lines.next() else {
        return (content.to_string(), false);
    };

    if first_line.trim() != "---" {
        return (content.to_string(), false);
    }

    let mut output = String::new();
    output.push_str(first_line);
    output.push('\n');

    let mut replaced = false;
    let mut frontmatter_ended = false;

    for line in lines {
        if line.trim() == "---" && !frontmatter_ended {
            if !replaced {
                output.push_str("canonical: true\n");
                replaced = true;
            }

            output.push_str("---\n");
            frontmatter_ended = true;
            continue;
        }

        if !frontmatter_ended && line.trim_start().starts_with("canonical:") {
            output.push_str("canonical: true\n");
            replaced = true;
            continue;
        }

        output.push_str(line);
        output.push('\n');
    }

    let output = output.trim_end_matches('\n').to_string() + "\n";
    let changed = output != content;

    (output, changed)
}

fn build_note(root: &Path, path: &Path, stats: &mut IndexStats) -> std::io::Result<NoteRecord> {
    let original = fs::read_to_string(path)?;
    let relative = relative_path(root, path);

    let parsed = parse(&original);

    let default_namespace = infer_namespace(&relative);
    let default_note_type = infer_note_type(&relative);
    let default_title = infer_title(&parsed.body, &relative);
    let default_canonical = infer_canonical(&relative, default_note_type);

    let changed_content;

    if !parsed.had_frontmatter || parsed.metadata.id.is_none() {
        let now = now_utc();

        let (updated, changed) = add_missing_frontmatter(
            &original,
            &relative,
            &default_title,
            default_note_type,
            default_namespace,
            default_canonical,
            &now,
        );

        if changed {
            fs::write(path, &updated)?;

            if parsed.had_frontmatter {
                stats.frontmatter_updated += 1;
            } else {
                stats.frontmatter_created += 1;
            }

            changed_content = updated;
        } else {
            changed_content = original.clone();
        }
    } else {
        changed_content = original.clone();
    }

    let (changed_content, canonical_changed) =
        enforce_system_canonical(&relative, &changed_content);

    if canonical_changed {
        fs::write(path, &changed_content)?;
        stats.frontmatter_updated += 1;
    }

    let parsed = parse(&changed_content);

    let id = parsed
        .metadata
        .id
        .expect("indexer failed to establish Note ID");

    let title = parsed
        .metadata
        .title
        .unwrap_or_else(|| default_title.clone());

    let note_type = parsed
        .metadata
        .note_type
        .unwrap_or_else(|| default_note_type.to_string());

    let namespace = parsed
        .metadata
        .namespace
        .unwrap_or_else(|| default_namespace.to_string());

    let canonical = parsed.metadata.canonical.unwrap_or(default_canonical);

    let status = parsed
        .metadata
        .status
        .unwrap_or_else(|| "active".to_string());

    let checksum = sha256_hex(&changed_content);

    Ok(NoteRecord {
        id,
        path: relative,
        title,
        note_type,
        namespace,
        canonical,
        status,
        memory_kind: parsed.metadata.memory_kind,
        task_status: parsed.metadata.task_status,
        reminder_status: parsed.metadata.reminder_status,

        checksum,

        captured_at: parsed.metadata.captured_at,
        occurred_at: parsed.metadata.occurred_at,
        valid_from: parsed.metadata.valid_from,
        valid_to: parsed.metadata.valid_to,
        due_at: parsed.metadata.due_at,
        updated_at: parsed.metadata.updated_at,
        created_at: parsed.metadata.created_at,

        body: parsed.body,

        entities: parsed.entities,
        relationships: parsed.relationships,
        events: parsed.events,
        states: parsed.states,
    })
}

fn memory_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(value) = env::var_os("ASSISTANT_MEMORY_ROOT") {
        return Ok(PathBuf::from(value));
    }

    let home = env::var_os("HOME").ok_or("HOME environment variable is not set")?;

    Ok(PathBuf::from(home).join("assistant-memory"))
}

fn infer_canonical(path: &str, note_type: &str) -> bool {
    matches!(path, "ASSISTANT.md" | "ENVIRONMENT.md") || note_type == "canonical"
}

pub fn build_snapshot() -> Result<IndexSnapshot, Box<dyn std::error::Error>> {
    let root = memory_root()?;
    build_snapshot_from(root)
}

pub fn build_snapshot_from(
    root: impl AsRef<Path>,
) -> Result<IndexSnapshot, Box<dyn std::error::Error>> {
    let root = root.as_ref().to_path_buf();

    if !root.is_dir() {
        return Err(format!("Memory root does not exist: {}", root.display()).into());
    }

    let mut paths = Vec::new();
    discover(&root, &mut paths)
        .map_err(|error| format!("indexer discover failed for {}: {}", root.display(), error))?;
    paths.sort();

    let mut stats = IndexStats::default();
    let mut notes = Vec::new();
    let mut chunks = Vec::<crate::model::ChunkRecord>::new();

    for path in paths {
        let relative = relative_path(&root, &path);

        if is_generated_note(&relative) {
            continue;
        }

        let note = build_note(&root, &path, &mut stats).map_err(|error| {
            format!(
                "indexer build_note failed for {}: {}",
                path.display(),
                error
            )
        })?;

        if !matches!(note.note_type.as_str(), "index" | "timeline") {
            chunks.extend(chunker::chunk_markdown(&note.id, &note.body));
        }

        stats.notes += 1;
        notes.push(note);
    }

    stats.chunks = chunks.len();

    indexes::write_global_index(&root, &notes).map_err(|error| {
        format!(
            "indexer write_global_index failed for {}: {}",
            root.display(),
            error
        )
    })?;

    indexes::write_namespace_indexes(&root, &notes).map_err(|error| {
        format!(
            "indexer write_namespace_indexes failed for {}: {}",
            root.display(),
            error
        )
    })?;

    indexes::write_timeline(&root, &notes).map_err(|error| {
        format!(
            "indexer write_timeline failed for {}: {}",
            root.display(),
            error
        )
    })?;

    Ok(IndexSnapshot {
        notes,
        chunks,
        stats,
    })
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = build_snapshot()?;
    let stats = &snapshot.stats;

    println!("Markdown index complete.");
    println!("Memory root: {}", memory_root()?.display());
    println!("Notes: {}", stats.notes);
    println!("Chunks: {}", stats.chunks);
    println!("Frontmatter created: {}", stats.frontmatter_created);
    println!("Frontmatter updated: {}", stats.frontmatter_updated);

    Ok(())
}

#[cfg(test)]
mod system_canonical_tests {
    use super::enforce_system_canonical;

    #[test]
    fn system_notes_are_always_canonical() {
        let input = r#"---
id: "test-id"
title: "Assistant"
canonical: false
status: active
---

# Assistant
"#;

        let (output, changed) = enforce_system_canonical("ASSISTANT.md", input);

        assert!(changed);
        assert!(output.contains("canonical: true"));
        assert!(!output.contains("canonical: false"));
    }

    #[test]
    fn ordinary_notes_are_not_modified() {
        let input = r#"---
id: "test-id"
title: "Example"
canonical: false
status: active
---

# Example
"#;

        let (output, changed) = enforce_system_canonical("journal/example.md", input);

        assert!(!changed);
        assert_eq!(output, input);
    }
}

#[derive(Debug)]
pub struct IndexSnapshot {
    pub notes: Vec<crate::model::NoteRecord>,
    pub chunks: Vec<crate::model::ChunkRecord>,
    pub stats: crate::model::IndexStats,
}
