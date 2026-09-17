use crate::hashing::sha256_hex;
use crate::model::ChunkRecord;

const MAX_CHUNK_CHARS: usize = 1800;

fn heading_level(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();

    let mut count = 0usize;

    while count < bytes.len() && bytes[count] == b'#' {
        count += 1;
    }

    if (1..=6).contains(&count) && bytes.get(count) == Some(&b' ') {
        Some(count)
    } else {
        None
    }
}

fn heading_title(line: &str) -> String {
    line.trim_start_matches('#').trim().to_string()
}

fn split_long_block(text: &str) -> Vec<String> {
    if text.len() <= MAX_CHUNK_CHARS {
        return vec![text.to_string()];
    }

    let mut pieces = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let separator_len = usize::from(!current.is_empty());

        if current.len() + separator_len + word.len() > MAX_CHUNK_CHARS && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
        }

        if !current.is_empty() {
            current.push(' ');
        }

        current.push_str(word);
    }

    if !current.is_empty() {
        pieces.push(current);
    }

    pieces
}

pub fn chunk_markdown(note_id: &str, body: &str) -> Vec<ChunkRecord> {
    let mut chunks = Vec::new();

    let mut heading_stack: Vec<String> = Vec::new();
    let mut current_lines: Vec<(usize, String)> = Vec::new();

    let mut in_fence = false;
    let mut ordinal = 0u64;

    fn flush(
        chunks: &mut Vec<ChunkRecord>,
        current_lines: &mut Vec<(usize, String)>,
        heading_stack: &[String],
        note_id: &str,
        ordinal: &mut u64,
    ) {
        if current_lines.is_empty() {
            return;
        }

        let start_line = current_lines.first().map(|(line, _)| *line).unwrap_or(1);

        let end_line = current_lines
            .last()
            .map(|(line, _)| *line)
            .unwrap_or(start_line);

        let raw = current_lines
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();

        current_lines.clear();

        if raw.is_empty() {
            return;
        }

        let anchor = heading_stack.join(" > ");

        let contextual = if anchor.is_empty() {
            raw
        } else {
            format!("{anchor}\n\n{raw}")
        };

        for piece in split_long_block(&contextual) {
            let content_hash = sha256_hex(&piece);

            let identity = format!("{note_id}\n{}\n{content_hash}", *ordinal);

            let id = sha256_hex(identity);

            chunks.push(ChunkRecord {
                id,
                note_id: note_id.to_string(),
                ordinal: *ordinal,
                text: piece,
                content_hash,
                source_anchor: anchor.clone(),
                source_start_line: start_line,
                source_end_line: end_line,
            });

            *ordinal += 1;
        }
    }

    for (index, line) in body.lines().enumerate() {
        let line_number = index + 1;

        if let Some(level) = heading_level(line) {
            flush(
                &mut chunks,
                &mut current_lines,
                &heading_stack,
                note_id,
                &mut ordinal,
            );

            heading_stack.truncate(level.saturating_sub(1));
            heading_stack.push(heading_title(line));

            continue;
        }

        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }

        if line.trim().is_empty() && !in_fence {
            flush(
                &mut chunks,
                &mut current_lines,
                &heading_stack,
                note_id,
                &mut ordinal,
            );

            continue;
        }

        current_lines.push((line_number, line.to_string()));
    }

    flush(
        &mut chunks,
        &mut current_lines,
        &heading_stack,
        note_id,
        &mut ordinal,
    );

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_heading_aware_chunks() {
        let body = "# Local AI\n\nIntro.\n\n## Qwen\n\nFast enough.";

        let chunks = chunk_markdown("note-1", body);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("Local AI"));
        assert!(chunks[1].text.contains("Local AI > Qwen"));
    }

    #[test]
    fn chunk_ids_are_deterministic() {
        let body = "# Local AI\n\nTest.";

        let first = chunk_markdown("note-1", body);
        let second = chunk_markdown("note-1", body);

        assert_eq!(first.len(), second.len());
        assert_eq!(first[0].id, second[0].id);
    }
}
