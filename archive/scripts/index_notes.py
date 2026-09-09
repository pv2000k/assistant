from pathlib import Path
import sqlite3
from datetime import datetime

NOTES_DIR = Path.home() / "assistant" / "notes"
DB_PATH = Path.home() / "assistant" / "data" / "assistant.db"


def parse_frontmatter(text: str) -> tuple[dict, str]:
    if not text.startswith("---"):
        return {}, text

    parts = text.split("---", 2)

    if len(parts) != 3:
        return {}, text

    frontmatter = parts[1]
    body = parts[2].lstrip()

    data = {
        "tags": [],
        "entities": [],
        "relationships": [],
        "events": [],
        "states": [],
    }

    section = None
    current_entity = None
    current_relationship = None
    current_event = None
    current_state = None
    event_entities_mode = False

    for raw_line in frontmatter.splitlines():
        line = raw_line.strip()

        if not line:
            continue

        # ---------------------------------------------------------
        # TOP-LEVEL SECTIONS
        # ---------------------------------------------------------

        if line == "tags:" and section != "events":
            section = "tags"
            event_entities_mode = False
            continue

        if line == "entities:" and section != "events":
            section = "entities"
            current_entity = None
            event_entities_mode = False
            continue

        if line == "relationships:" and section != "events":
            section = "relationships"
            current_relationship = None
            event_entities_mode = False
            continue

        if line == "events:":
            section = "events"
            current_event = None
            event_entities_mode = False
            continue

        if line == "states:":
            section = "states"
            current_state = None
            event_entities_mode = False
            continue

        # ---------------------------------------------------------
        # TOP-LEVEL NOTE METADATA
        # ---------------------------------------------------------

        if line.startswith("title:"):
            data["title"] = line.split(":", 1)[1].strip()
            continue

        if (
            line.startswith("type:")
            and section not in {
                "entities",
                "relationships",
                "events",
                "states",
            }
        ):
            data["type"] = line.split(":", 1)[1].strip()
            continue

        # ---------------------------------------------------------
        # TAGS
        # ---------------------------------------------------------

        if section == "tags":
            if line.startswith("- "):
                data["tags"].append(line[2:].strip())
            continue

        # ---------------------------------------------------------
        # ENTITIES
        # ---------------------------------------------------------

        if section == "entities":
            if line.startswith("- name:"):
                current_entity = {
                    "name": line.split(":", 1)[1].strip(),
                    "type": "unknown",
                }
                data["entities"].append(current_entity)
                continue

            if line.startswith("type:") and current_entity is not None:
                current_entity["type"] = line.split(
                    ":", 1
                )[1].strip()
                continue

        # ---------------------------------------------------------
        # RELATIONSHIPS
        # ---------------------------------------------------------

        if section == "relationships":
            if line.startswith("- source:"):
                current_relationship = {
                    "source": line.split(":", 1)[1].strip(),
                    "target": None,
                    "relationship": None,
                }
                data["relationships"].append(
                    current_relationship
                )
                continue

            if (
                line.startswith("target:")
                and current_relationship is not None
            ):
                current_relationship["target"] = line.split(
                    ":", 1
                )[1].strip()
                continue

            if (
                line.startswith("relationship:")
                and current_relationship is not None
            ):
                current_relationship["relationship"] = line.split(
                    ":", 1
                )[1].strip()
                continue

        # ---------------------------------------------------------
        # EVENTS
        # ---------------------------------------------------------

        if section == "events":
            # Nested "entities:" belongs to the current event.
            if line == "entities:" and current_event is not None:
                event_entities_mode = True
                continue

            if line.startswith("- type:"):
                current_event = {
                    "type": line.split(":", 1)[1].strip(),
                    "occurred_at": None,
                    "description": None,
                    "entities": [],
                }
                data["events"].append(current_event)
                event_entities_mode = False
                continue

            if current_event is None:
                continue

            if event_entities_mode and line.startswith("- "):
                current_event["entities"].append(
                    line[2:].strip()
                )
                continue

            if line.startswith("occurred_at:"):
                current_event["occurred_at"] = line.split(
                    ":", 1
                )[1].strip()
                event_entities_mode = False
                continue

            if line.startswith("description:"):
                current_event["description"] = line.split(
                    ":", 1
                )[1].strip()
                event_entities_mode = False
                continue

        # ---------------------------------------------------------
        # STATES
        # ---------------------------------------------------------

        if section == "states":
            if line.startswith("- entity:"):
                current_state = {
                    "entity": line.split(":", 1)[1].strip(),
                    "state": None,
                    "valid_from": None,
                    "valid_to": None,
                }
                data["states"].append(current_state)
                continue

            if current_state is None:
                continue

            if line.startswith("state:"):
                current_state["state"] = line.split(
                    ":", 1
                )[1].strip()
                continue

            if line.startswith("valid_from:"):
                current_state["valid_from"] = line.split(
                    ":", 1
                )[1].strip()
                continue

            if line.startswith("valid_to:"):
                current_state["valid_to"] = line.split(
                    ":", 1
                )[1].strip()
                continue

    return data, body


def filesystem_times(path: Path) -> tuple[str, str]:
    stat = path.stat()

    created_timestamp = getattr(
        stat,
        "st_birthtime",
        stat.st_ctime,
    )

    return (
        datetime.fromtimestamp(
            created_timestamp
        ).isoformat(),
        datetime.fromtimestamp(
            stat.st_mtime
        ).isoformat(),
    )


def ensure_entity(
    conn: sqlite3.Connection,
    name: str,
    entity_type: str = "unknown",
) -> int:
    conn.execute(
        """
        INSERT INTO entities (name, type)
        VALUES (?, ?)
        ON CONFLICT(name) DO NOTHING
        """,
        (name, entity_type),
    )

    row = conn.execute(
        """
        SELECT id
        FROM entities
        WHERE name = ?
        """,
        (name,),
    ).fetchone()

    if row is None:
        raise RuntimeError(
            f"Failed to create/find entity: {name}"
        )

    return row[0]


def main() -> None:
    markdown_files = sorted(
        NOTES_DIR.rglob("*.md")
    )

    parsed_notes = []

    for path in markdown_files:
        text = path.read_text(
            encoding="utf-8"
        )

        metadata, body = parse_frontmatter(text)

        created_at, updated_at = filesystem_times(
            path
        )

        parsed_notes.append(
            {
                "path": str(
                    path.relative_to(NOTES_DIR)
                ),
                "title": metadata.get(
                    "title",
                    path.stem,
                ),
                "type": metadata.get(
                    "type",
                    "unknown",
                ),
                "created_at": created_at,
                "updated_at": updated_at,
                "tags": metadata.get(
                    "tags",
                    [],
                ),
                "entities": metadata.get(
                    "entities",
                    [],
                ),
                "relationships": metadata.get(
                    "relationships",
                    [],
                ),
                "events": metadata.get(
                    "events",
                    [],
                ),
                "states": metadata.get(
                    "states",
                    [],
                ),
                "body": body,
            }
        )

    conn = sqlite3.connect(DB_PATH)
    conn.execute(
        "PRAGMA foreign_keys = ON"
    )

    try:
        with conn:
            current_paths = {
                note["path"]
                for note in parsed_notes
            }

            existing_paths = conn.execute(
                "SELECT path FROM notes"
            ).fetchall()

            # Remove notes no longer present.
            for (path,) in existing_paths:
                if path not in current_paths:
                    conn.execute(
                        "DELETE FROM notes WHERE path = ?",
                        (path,),
                    )

            # -----------------------------------------------------
            # NOTES
            # -----------------------------------------------------

            conn.execute(
                "DELETE FROM note_tags"
            )

            conn.execute(
                "DELETE FROM note_fts"
            )

            for note in parsed_notes:
                conn.execute(
                    """
                    INSERT INTO notes
                        (
                            path,
                            title,
                            type,
                            created_at,
                            updated_at
                        )
                    VALUES (?, ?, ?, ?, ?)
                    ON CONFLICT(path) DO UPDATE SET
                        title = excluded.title,
                        type = excluded.type,
                        created_at = excluded.created_at,
                        updated_at = excluded.updated_at
                    """,
                    (
                        note["path"],
                        note["title"],
                        note["type"],
                        note["created_at"],
                        note["updated_at"],
                    ),
                )

                note_id = conn.execute(
                    """
                    SELECT id
                    FROM notes
                    WHERE path = ?
                    """,
                    (note["path"],),
                ).fetchone()[0]

                # FTS
                conn.execute(
                    """
                    INSERT INTO note_fts
                        (note_id, title, body)
                    VALUES (?, ?, ?)
                    """,
                    (
                        note_id,
                        note["title"],
                        note["body"],
                    ),
                )

                # Tags
                for tag in note["tags"]:
                    conn.execute(
                        """
                        INSERT OR IGNORE INTO tags (name)
                        VALUES (?)
                        """,
                        (tag,),
                    )

                    tag_id = conn.execute(
                        """
                        SELECT id
                        FROM tags
                        WHERE name = ?
                        """,
                        (tag,),
                    ).fetchone()[0]

                    conn.execute(
                        """
                        INSERT OR IGNORE INTO note_tags
                            (note_id, tag_id)
                        VALUES (?, ?)
                        """,
                        (
                            note_id,
                            tag_id,
                        ),
                    )

            # Remove unused tags.
            conn.execute(
                """
                DELETE FROM tags
                WHERE id NOT IN (
                    SELECT DISTINCT tag_id
                    FROM note_tags
                )
                """
            )

            # -----------------------------------------------------
            # GRAPH
            # -----------------------------------------------------

            conn.execute(
                "DELETE FROM note_relationships"
            )
            conn.execute(
                "DELETE FROM note_entities"
            )
            conn.execute(
                "DELETE FROM relationships"
            )
            conn.execute(
                "DELETE FROM entities"
            )

            # Explicit entities.
            for note in parsed_notes:
                note_id = conn.execute(
                    """
                    SELECT id
                    FROM notes
                    WHERE path = ?
                    """,
                    (note["path"],),
                ).fetchone()[0]

                for entity in note["entities"]:
                    entity_id = ensure_entity(
                        conn,
                        entity["name"],
                        entity.get(
                            "type",
                            "unknown",
                        ),
                    )

                    conn.execute(
                        """
                        INSERT OR IGNORE INTO note_entities
                            (note_id, entity_id)
                        VALUES (?, ?)
                        """,
                        (
                            note_id,
                            entity_id,
                        ),
                    )

            # Relationships.
            for note in parsed_notes:
                note_id = conn.execute(
                    """
                    SELECT id
                    FROM notes
                    WHERE path = ?
                    """,
                    (note["path"],),
                ).fetchone()[0]

                for relationship in note[
                    "relationships"
                ]:
                    source = relationship.get(
                        "source"
                    )
                    target = relationship.get(
                        "target"
                    )
                    relationship_type = relationship.get(
                        "relationship"
                    )

                    if (
                        not source
                        or not target
                        or not relationship_type
                    ):
                        continue

                    source_id = ensure_entity(
                        conn,
                        source,
                    )

                    target_id = ensure_entity(
                        conn,
                        target,
                    )

                    conn.execute(
                        """
                        INSERT OR IGNORE INTO relationships
                            (
                                source_id,
                                target_id,
                                relationship
                            )
                        VALUES (?, ?, ?)
                        """,
                        (
                            source_id,
                            target_id,
                            relationship_type,
                        ),
                    )

                    relationship_id = conn.execute(
                        """
                        SELECT id
                        FROM relationships
                        WHERE source_id = ?
                          AND target_id = ?
                          AND relationship = ?
                        """,
                        (
                            source_id,
                            target_id,
                            relationship_type,
                        ),
                    ).fetchone()[0]

                    conn.execute(
                        """
                        INSERT OR IGNORE INTO note_relationships
                            (
                                note_id,
                                relationship_id
                            )
                        VALUES (?, ?)
                        """,
                        (
                            note_id,
                            relationship_id,
                        ),
                    )

            # -----------------------------------------------------
            # TEMPORAL EVENTS
            # -----------------------------------------------------

            conn.execute(
                "DELETE FROM event_entities"
            )
            conn.execute(
                "DELETE FROM temporal_events"
            )

            for note in parsed_notes:
                note_id = conn.execute(
                    """
                    SELECT id
                    FROM notes
                    WHERE path = ?
                    """,
                    (note["path"],),
                ).fetchone()[0]

                for event in note["events"]:
                    occurred_at = event.get(
                        "occurred_at"
                    )

                    if not occurred_at:
                        continue

                    cursor = conn.execute(
                        """
                        INSERT INTO temporal_events
                            (
                                note_id,
                                event_type,
                                occurred_at,
                                description
                            )
                        VALUES (?, ?, ?, ?)
                        """,
                        (
                            note_id,
                            event.get(
                                "type",
                                "unknown",
                            ),
                            occurred_at,
                            event.get(
                                "description"
                            ),
                        ),
                    )

                    event_id = cursor.lastrowid

                    for entity_name in event.get(
                        "entities",
                        [],
                    ):
                        entity_id = ensure_entity(
                            conn,
                            entity_name,
                        )

                        conn.execute(
                            """
                            INSERT OR IGNORE INTO event_entities
                                (
                                    event_id,
                                    entity_id,
                                    role
                                )
                            VALUES (?, ?, ?)
                            """,
                            (
                                event_id,
                                entity_id,
                                "about",
                            ),
                        )

            # -----------------------------------------------------
            # ENTITY STATES
            # -----------------------------------------------------

            conn.execute(
                "DELETE FROM entity_states"
            )

            for note in parsed_notes:
                note_id = conn.execute(
                    """
                    SELECT id
                    FROM notes
                    WHERE path = ?
                    """,
                    (note["path"],),
                ).fetchone()[0]

                for state in note["states"]:
                    entity_name = state.get(
                        "entity"
                    )
                    state_text = state.get(
                        "state"
                    )
                    valid_from = state.get(
                        "valid_from"
                    )

                    if not entity_name:
                        continue

                    if not state_text:
                        continue

                    if not valid_from:
                        continue

                    entity_id = ensure_entity(
                        conn,
                        entity_name,
                    )

                    conn.execute(
                        """
                        INSERT INTO entity_states
                            (
                                entity_id,
                                note_id,
                                state,
                                valid_from,
                                valid_to
                            )
                        VALUES (?, ?, ?, ?, ?)
                        """,
                        (
                            entity_id,
                            note_id,
                            state_text,
                            valid_from,
                            state.get(
                                "valid_to"
                            ),
                        ),
                    )

            for note in parsed_notes:
                print(
                    f"Indexed: {note['path']}"
                )

    finally:
        conn.close()


if __name__ == "__main__":
    main()