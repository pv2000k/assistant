from __future__ import annotations

import argparse
import json
import re
import sqlite3
from collections import deque
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Any

DB_PATH = Path.home() / "assistant" / "data" / "assistant.db"


# ---------------------------------------------------------------------------
# Database
# ---------------------------------------------------------------------------


def connect() -> sqlite3.Connection:
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    conn.execute("PRAGMA busy_timeout = 5000")
    return conn


def entity_info(row: sqlite3.Row | None) -> dict[str, Any] | None:
    if row is None:
        return None
    return {
        "id": row["id"],
        "name": row["name"],
        "type": row["type"],
    }


def find_entity(conn: sqlite3.Connection, name: str) -> sqlite3.Row | None:
    name = name.strip()
    if not name:
        return None

    row = conn.execute(
        """
        SELECT id, name, type
        FROM entities
        WHERE name = ? COLLATE NOCASE
        LIMIT 1
        """,
        (name,),
    ).fetchone()

    if row is not None:
        return row

    return conn.execute(
        """
        SELECT id, name, type
        FROM entities
        WHERE name LIKE ? COLLATE NOCASE
        ORDER BY
            CASE WHEN name LIKE ? COLLATE NOCASE THEN 0 ELSE 1 END,
            LENGTH(name),
            name
        LIMIT 1
        """,
        (f"%{name}%", f"{name}%"),
    ).fetchone()


def fetch_notes(
    conn: sqlite3.Connection,
    note_ids: set[int] | list[int] | tuple[int, ...],
) -> dict[int, sqlite3.Row]:
    ids = sorted({int(note_id) for note_id in note_ids})
    if not ids:
        return {}

    placeholders = ",".join("?" for _ in ids)

    rows = conn.execute(
        f"""
        SELECT
            n.id,
            n.title,
            n.path,
            n.type,
            f.body
        FROM notes n
        JOIN note_fts f
            ON f.note_id = n.id
        WHERE n.id IN ({placeholders})
        """,
        ids,
    ).fetchall()

    return {row["id"]: row for row in rows}


# ---------------------------------------------------------------------------
# Full-text search
# ---------------------------------------------------------------------------


def search_tokens(query: str) -> list[str]:
    """Turn punctuation-heavy names such as Qwen3.5 or llama.cpp into FTS tokens."""
    return re.findall(r"[\w]+", query, flags=re.UNICODE)


def fts_query(query: str) -> str:
    tokens = search_tokens(query)
    if not tokens:
        return ""

    # FTS5 tokenizes punctuation as separators, so AND the normalized tokens.
    # Example: Qwen3.5 -> "Qwen3" AND "5".
    return " AND ".join(
        f'"{token.replace(chr(34), chr(34) * 2)}"'
        for token in tokens
    )


def search_notes(
    conn: sqlite3.Connection,
    query: str,
    limit: int = 20,
) -> dict[str, Any]:
    match_query = fts_query(query)

    if not match_query:
        return {"query": query, "results": []}

    rows = conn.execute(
        """
        SELECT
            note_id,
            title,
            snippet(
                note_fts,
                2,
                '[[',
                ']]',
                ' ... ',
                24
            ) AS snippet,
            bm25(note_fts) AS score
        FROM note_fts
        WHERE note_fts MATCH ?
        ORDER BY score
        LIMIT ?
        """,
        (match_query, limit),
    ).fetchall()

    note_map = fetch_notes(
        conn,
        [row["note_id"] for row in rows],
    )

    results = []
    for row in rows:
        note = note_map.get(row["note_id"])
        results.append(
            {
                "note_id": row["note_id"],
                "title": row["title"],
                "path": note["path"] if note else None,
                "type": note["type"] if note else None,
                "snippet": row["snippet"],
                "score": row["score"],
            }
        )

    return {
        "query": query,
        "results": results,
    }


# ---------------------------------------------------------------------------
# Entity-centric retrieval
# ---------------------------------------------------------------------------


def get_thoughts(
    conn: sqlite3.Connection,
    entity_name: str,
    limit: int = 20,
) -> dict[str, Any]:
    entity = find_entity(conn, entity_name)

    if entity is None:
        return {"entity": entity_name, "results": []}

    rows = conn.execute(
        """
        SELECT
            e.occurred_at,
            e.event_type,
            e.description,
            n.title AS note_title,
            n.path AS note_path
        FROM temporal_events e
        JOIN event_entities ee
            ON ee.event_id = e.id
        JOIN notes n
            ON n.id = e.note_id
        WHERE ee.entity_id = ?
          AND LOWER(e.event_type) IN (
              'thought',
              'idea',
              'decision',
              'reflection'
          )
        ORDER BY e.occurred_at DESC
        LIMIT ?
        """,
        (entity["id"], limit),
    ).fetchall()

    return {
        "entity": entity_info(entity),
        "results": [dict(row) for row in rows],
    }


def get_timeline(
    conn: sqlite3.Connection,
    entity_name: str,
    limit: int = 50,
) -> dict[str, Any]:
    entity = find_entity(conn, entity_name)

    if entity is None:
        return {"entity": entity_name, "results": []}

    rows = conn.execute(
        """
        SELECT
            e.occurred_at,
            e.event_type,
            e.description,
            n.title AS note_title,
            n.path AS note_path
        FROM temporal_events e
        JOIN event_entities ee
            ON ee.event_id = e.id
        JOIN notes n
            ON n.id = e.note_id
        WHERE ee.entity_id = ?
        ORDER BY e.occurred_at ASC
        LIMIT ?
        """,
        (entity["id"], limit),
    ).fetchall()

    return {
        "entity": entity_info(entity),
        "results": [dict(row) for row in rows],
    }


def parse_date(value: str) -> date:
    try:
        return date.fromisoformat(value)
    except ValueError as exc:
        raise ValueError(
            f"Invalid date '{value}'. Use YYYY-MM-DD."
        ) from exc


def get_state(
    conn: sqlite3.Connection,
    entity_name: str,
    as_of: str | None = None,
) -> dict[str, Any]:
    entity = find_entity(conn, entity_name)

    if entity is None:
        return {
            "entity": entity_name,
            "state": None,
        }

    if as_of is None:
        as_of = datetime.now().date().isoformat()
    else:
        parse_date(as_of)

    row = conn.execute(
        """
        SELECT
            s.state,
            s.valid_from,
            s.valid_to,
            n.title AS note_title,
            n.path AS note_path
        FROM entity_states s
        JOIN notes n
            ON n.id = s.note_id
        WHERE s.entity_id = ?
          AND s.valid_from <= ?
          AND (
              s.valid_to IS NULL
              OR s.valid_to >= ?
          )
        ORDER BY s.valid_from DESC
        LIMIT 1
        """,
        (entity["id"], as_of, as_of),
    ).fetchone()

    return {
        "entity": entity_info(entity),
        "as_of": as_of,
        "state": dict(row) if row else None,
    }


def get_related(
    conn: sqlite3.Connection,
    entity_name: str,
    limit: int = 20,
) -> dict[str, Any]:
    entity = find_entity(conn, entity_name)

    if entity is None:
        return {
            "entity": entity_name,
            "entities": [],
            "notes": [],
        }

    related_entities = conn.execute(
        """
        SELECT
            e.id,
            e.name,
            e.type,
            r.relationship,
            'outgoing' AS direction
        FROM relationships r
        JOIN entities e
            ON e.id = r.target_id
        WHERE r.source_id = ?

        UNION ALL

        SELECT
            e.id,
            e.name,
            e.type,
            r.relationship,
            'incoming' AS direction
        FROM relationships r
        JOIN entities e
            ON e.id = r.source_id
        WHERE r.target_id = ?

        ORDER BY name
        LIMIT ?
        """,
        (entity["id"], entity["id"], limit),
    ).fetchall()

    note_rows = conn.execute(
        """
        SELECT DISTINCT n.title, n.path
        FROM notes n
        JOIN note_entities ne
            ON ne.note_id = n.id
        WHERE ne.entity_id = ?

        UNION

        SELECT DISTINCT n.title, n.path
        FROM notes n
        JOIN temporal_events te
            ON te.note_id = n.id
        JOIN event_entities ee
            ON ee.event_id = te.id
        WHERE ee.entity_id = ?

        UNION

        SELECT DISTINCT n.title, n.path
        FROM notes n
        JOIN entity_states es
            ON es.note_id = n.id
        WHERE es.entity_id = ?

        ORDER BY path
        LIMIT ?
        """,
        (entity["id"], entity["id"], entity["id"], limit),
    ).fetchall()

    return {
        "entity": entity_info(entity),
        "entities": [dict(row) for row in related_entities],
        "notes": [dict(row) for row in note_rows],
    }


def get_context(
    conn: sqlite3.Connection,
    query: str,
    limit: int = 5,
) -> dict[str, Any]:
    """Simple entity-centric context retrieval."""
    entity = find_entity(conn, query)

    search = search_notes(conn, query, limit)

    note_ids = {
        result["note_id"]
        for result in search["results"]
    }

    if entity is not None:
        related = get_related(conn, query, limit)
        related_paths = {
            note["path"]
            for note in related["notes"]
        }

        if related_paths:
            rows = conn.execute(
                """
                SELECT id
                FROM notes
                WHERE path IN ({})
                """.format(
                    ",".join("?" for _ in related_paths)
                ),
                tuple(related_paths),
            ).fetchall()
            note_ids.update(row["id"] for row in rows)
    else:
        related = {
            "entities": [],
            "notes": [],
        }

    note_map = fetch_notes(conn, note_ids)

    source_notes = []
    for note_id, row in sorted(
        note_map.items(),
        key=lambda item: item[1]["path"],
    ):
        body = row["body"]
        if len(body) > 1800:
            body = body[:1800].rstrip() + "\n[...]"

        source_notes.append(
            {
                "note_id": note_id,
                "title": row["title"],
                "path": row["path"],
                "type": row["type"],
                "content": body,
            }
        )

        if len(source_notes) >= limit:
            break

    if entity is None:
        return {
            "query": query,
            "entity": None,
            "thoughts": [],
            "timeline": [],
            "current_state": None,
            "related_entities": [],
            "source_notes": source_notes,
        }

    return {
        "query": query,
        "entity": entity_info(entity),
        "thoughts": get_thoughts(conn, query, limit)["results"],
        "timeline": get_timeline(conn, query, limit)["results"],
        "current_state": get_state(conn, query)["state"],
        "related_entities": related["entities"],
        "source_notes": source_notes,
    }


# ---------------------------------------------------------------------------
# Time-centric retrieval
# ---------------------------------------------------------------------------


def get_time_context(
    conn: sqlite3.Connection,
    start: str,
    end: str | None = None,
    limit: int = 50,
) -> dict[str, Any]:
    start_date = parse_date(start)
    end_date = parse_date(end) if end else start_date

    if end_date < start_date:
        raise ValueError("End date cannot be before start date.")

    end_exclusive = (
        end_date + timedelta(days=1)
    ).isoformat()

    rows = conn.execute(
        """
        SELECT
            e.id,
            e.occurred_at,
            e.event_type,
            e.description,
            n.id AS note_id,
            n.title AS note_title,
            n.path AS note_path,
            GROUP_CONCAT(DISTINCT ent.name) AS entities
        FROM temporal_events e
        JOIN notes n
            ON n.id = e.note_id
        LEFT JOIN event_entities ee
            ON ee.event_id = e.id
        LEFT JOIN entities ent
            ON ent.id = ee.entity_id
        WHERE e.occurred_at >= ?
          AND e.occurred_at < ?
        GROUP BY e.id
        ORDER BY e.occurred_at ASC
        LIMIT ?
        """,
        (start_date.isoformat(), end_exclusive, limit),
    ).fetchall()

    events = []
    for row in rows:
        event = dict(row)
        event["entities"] = (
            event["entities"].split(",")
            if event["entities"]
            else []
        )
        events.append(event)

    state_rows = conn.execute(
        """
        SELECT
            s.state,
            s.valid_from,
            s.valid_to,
            ent.name AS entity,
            n.title AS note_title,
            n.path AS note_path
        FROM entity_states s
        JOIN entities ent
            ON ent.id = s.entity_id
        JOIN notes n
            ON n.id = s.note_id
        WHERE s.valid_from < ?
          AND (
              s.valid_to IS NULL
              OR s.valid_to >= ?
          )
        ORDER BY s.valid_from ASC
        """,
        (end_exclusive, start_date.isoformat()),
    ).fetchall()

    note_ids = {event["note_id"] for event in events}
    note_ids.update(row["note_id"] for row in state_rows)
    note_map = fetch_notes(conn, note_ids)

    source_notes = []
    for note_id in sorted(note_ids):
        row = note_map.get(note_id)
        if row is None:
            continue

        body = row["body"]
        if len(body) > 1800:
            body = body[:1800].rstrip() + "\n[...]"

        source_notes.append(
            {
                "note_id": row["id"],
                "title": row["title"],
                "path": row["path"],
                "type": row["type"],
                "content": body,
            }
        )

    return {
        "period": {
            "start": start_date.isoformat(),
            "end": end_date.isoformat(),
        },
        "events": events,
        "states": [dict(row) for row in state_rows],
        "source_notes": source_notes,
    }


# ---------------------------------------------------------------------------
# Graph traversal
# ---------------------------------------------------------------------------


def get_graph(
    conn: sqlite3.Connection,
    entity_name: str,
    depth: int = 2,
    limit: int = 50,
) -> dict[str, Any]:
    """Breadth-first graph traversal with both directions and cross-links."""
    if depth < 0:
        raise ValueError("Graph depth cannot be negative.")

    entity = find_entity(conn, entity_name)

    if entity is None:
        return {
            "entity": entity_name,
            "found": False,
            "depth": depth,
            "nodes": [],
            "relationships": [],
        }

    edge_rows = conn.execute(
        """
        SELECT
            r.id,
            r.source_id,
            r.target_id,
            r.relationship,
            source.name AS source_name,
            target.name AS target_name,
            source.type AS source_type,
            target.type AS target_type
        FROM relationships r
        JOIN entities source
            ON source.id = r.source_id
        JOIN entities target
            ON target.id = r.target_id
        """
    ).fetchall()

    adjacency: dict[int, list[tuple[int, int, str]]] = {}

    for row in edge_rows:
        adjacency.setdefault(row["source_id"], []).append(
            (
                row["target_id"],
                row["id"],
                "outgoing",
            )
        )
        adjacency.setdefault(row["target_id"], []).append(
            (
                row["source_id"],
                row["id"],
                "incoming",
            )
        )

    entity_rows = conn.execute(
        "SELECT id, name, type FROM entities"
    ).fetchall()
    entity_map = {
        row["id"]: row
        for row in entity_rows
    }

    root_id = entity["id"]
    distances: dict[int, int] = {root_id: 0}
    queue = deque([root_id])

    while queue and len(distances) < limit:
        current_id = queue.popleft()
        current_depth = distances[current_id]

        if current_depth >= depth:
            continue

        for neighbor_id, _, _ in adjacency.get(current_id, []):
            if neighbor_id in distances:
                continue

            distances[neighbor_id] = current_depth + 1
            queue.append(neighbor_id)

            if len(distances) >= limit:
                break

    nodes = [
        {
            "depth": distance,
            "id": node_id,
            "name": entity_map[node_id]["name"],
            "type": entity_map[node_id]["type"],
        }
        for node_id, distance in distances.items()
        if node_id in entity_map
    ]

    node_ids = set(distances)
    relationships = []

    for row in edge_rows:
        source_id = row["source_id"]
        target_id = row["target_id"]

        if source_id not in node_ids or target_id not in node_ids:
            continue

        source_depth = distances[source_id]
        target_depth = distances[target_id]

        if source_depth < target_depth:
            traversal_direction = "outgoing"
        elif source_depth > target_depth:
            traversal_direction = "incoming"
        else:
            traversal_direction = "cross_link"

        relationships.append(
            {
                "source": row["source_name"],
                "relationship": row["relationship"],
                "target": row["target_name"],
                "direction": traversal_direction,
                "source_depth": source_depth,
                "target_depth": target_depth,
            }
        )

    nodes.sort(key=lambda item: (item["depth"], item["name"].lower()))
    relationships.sort(
        key=lambda item: (
            min(item["source_depth"], item["target_depth"]),
            item["source"].lower(),
            item["target"].lower(),
        )
    )

    return {
        "entity": entity_info(entity),
        "found": True,
        "depth": depth,
        "nodes": nodes,
        "relationships": relationships,
    }


# ---------------------------------------------------------------------------
# Hybrid retrieval
# ---------------------------------------------------------------------------


def get_hybrid_context(
    conn: sqlite3.Connection,
    query: str,
    start: str | None = None,
    end: str | None = None,
    limit: int = 10,
    graph_depth: int = 2,
) -> dict[str, Any]:
    """Combine text, graph, temporal events and states into ranked evidence."""
    if graph_depth < 0:
        raise ValueError("Graph depth cannot be negative.")

    start_date = parse_date(start) if start else None
    end_date = parse_date(end) if end else None

    if start_date and end_date and end_date < start_date:
        raise ValueError("End date cannot be before start date.")

    lower_date = (
        start_date.isoformat()
        if start_date
        else "0001-01-01"
    )
    upper_date = (
        (end_date + timedelta(days=1)).isoformat()
        if end_date
        else "9999-12-31T23:59:59"
    )
    has_time_filter = start_date is not None or end_date is not None

    entity = find_entity(conn, query)

    graph = get_graph(
        conn,
        query,
        depth=graph_depth,
        limit=max(limit * 10, 50),
    )

    graph_nodes = {
        node["id"]: node
        for node in graph.get("nodes", [])
    }

    evidence: list[dict[str, Any]] = []

    def add_evidence(
        kind: str,
        strength: float,
        graph_distance: int,
        content: str,
        note_id: int | None = None,
        note_title: str | None = None,
        note_path: str | None = None,
        occurred_at: str | None = None,
        entity_name: str | None = None,
    ) -> None:
        evidence.append(
            {
                "kind": kind,
                "strength": strength,
                "graph_distance": graph_distance,
                "content": content,
                "note_id": note_id,
                "note_title": note_title,
                "note_path": note_path,
                "occurred_at": occurred_at,
                "entity": entity_name,
            }
        )

    # ---------------------------------------------------------
    # Full-text evidence
    # ---------------------------------------------------------

    match_query = fts_query(query)
    fts_rows = []

    if match_query:
        fts_rows = conn.execute(
            """
            SELECT
                note_id,
                title,
                body,
                bm25(note_fts) AS score
            FROM note_fts
            WHERE note_fts MATCH ?
            ORDER BY score
            LIMIT ?
            """,
            (match_query, limit * 5),
        ).fetchall()

    fts_note_ids = {row["note_id"] for row in fts_rows}
    fts_notes = fetch_notes(conn, fts_note_ids)

    for rank, row in enumerate(fts_rows):
        note = fts_notes.get(row["note_id"])
        if note is None:
            continue

        # Earlier FTS results receive slightly more weight.
        text_strength = max(
            6.0,
            12.0 - (rank * 0.5),
        )

        add_evidence(
            kind="full_text",
            strength=text_strength,
            graph_distance=0,
            content=row["body"],
            note_id=row["note_id"],
            note_title=row["title"],
            note_path=note["path"],
        )

    # ---------------------------------------------------------
    # Graph-linked notes
    # ---------------------------------------------------------

    graph_note_distance: dict[int, int] = {}

    for node in graph_nodes.values():
        entity_id = node["id"]
        distance = node["depth"]

        rows = conn.execute(
            """
            SELECT DISTINCT note_id
            FROM note_entities
            WHERE entity_id = ?

            UNION

            SELECT DISTINCT te.note_id
            FROM temporal_events te
            JOIN event_entities ee
                ON ee.event_id = te.id
            WHERE ee.entity_id = ?

            UNION

            SELECT DISTINCT es.note_id
            FROM entity_states es
            WHERE es.entity_id = ?
            """,
            (entity_id, entity_id, entity_id),
        ).fetchall()

        for row in rows:
            note_id = row["note_id"]
            previous = graph_note_distance.get(note_id)
            if previous is None or distance < previous:
                graph_note_distance[note_id] = distance

    # ---------------------------------------------------------
    # Temporal evidence
    # ---------------------------------------------------------

    event_best: dict[int, dict[str, Any]] = {}
    state_best: dict[int, dict[str, Any]] = {}

    for node in graph_nodes.values():
        entity_id = node["id"]
        distance = node["depth"]

        event_rows = conn.execute(
            """
            SELECT
                e.id,
                e.occurred_at,
                e.event_type,
                e.description,
                e.note_id,
                n.title AS note_title,
                n.path AS note_path
            FROM temporal_events e
            JOIN event_entities ee
                ON ee.event_id = e.id
            JOIN notes n
                ON n.id = e.note_id
            WHERE ee.entity_id = ?
              AND e.occurred_at >= ?
              AND e.occurred_at < ?
            ORDER BY e.occurred_at DESC
            LIMIT ?
            """,
            (entity_id, lower_date, upper_date, limit * 5),
        ).fetchall()

        for row in event_rows:
            strength = 16.0 / (distance + 1.0)
            current = event_best.get(row["id"])
            if current is None or strength > current["strength"]:
                event_best[row["id"]] = {
                    "row": row,
                    "strength": strength,
                    "graph_distance": distance,
                    "entity": node["name"],
                }

        state_rows = conn.execute(
            """
            SELECT
                s.id,
                s.state,
                s.valid_from,
                s.valid_to,
                s.note_id,
                ent.name AS entity,
                n.title AS note_title,
                n.path AS note_path
            FROM entity_states s
            JOIN entities ent
                ON ent.id = s.entity_id
            JOIN notes n
                ON n.id = s.note_id
            WHERE s.entity_id = ?
              AND s.valid_from <= ?
              AND (
                  s.valid_to IS NULL
                  OR s.valid_to >= ?
              )
            ORDER BY s.valid_from DESC
            LIMIT ?
            """,
            (entity_id, upper_date, lower_date, limit * 5),
        ).fetchall()

        for row in state_rows:
            strength = 13.0 / (distance + 1.0)
            current = state_best.get(row["id"])
            if current is None or strength > current["strength"]:
                state_best[row["id"]] = {
                    "row": row,
                    "strength": strength,
                    "graph_distance": distance,
                }

    for item in event_best.values():
        row = item["row"]
        add_evidence(
            kind="event",
            strength=item["strength"],
            graph_distance=item["graph_distance"],
            content=row["description"],
            note_id=row["note_id"],
            note_title=row["note_title"],
            note_path=row["note_path"],
            occurred_at=row["occurred_at"],
            entity_name=item["entity"],
        )

    for item in state_best.values():
        row = item["row"]
        add_evidence(
            kind="state",
            strength=item["strength"],
            graph_distance=item["graph_distance"],
            content=row["state"],
            note_id=row["note_id"],
            note_title=row["note_title"],
            note_path=row["note_path"],
            occurred_at=row["valid_from"],
            entity_name=row["entity"],
        )

    temporal_note_ids = {
        item["row"]["note_id"]
        for item in event_best.values()
    } | {
        item["row"]["note_id"]
        for item in state_best.values()
    }

    # ---------------------------------------------------------
    # Graph evidence
    # ---------------------------------------------------------

    graph_note_ids = set(graph_note_distance)

    # With a time filter, don't flood the result with arbitrary historical
    # graph notes. Keep FTS notes plus notes that have temporal evidence.
    if has_time_filter:
        graph_note_ids &= temporal_note_ids
        graph_note_ids.update(fts_note_ids)

    graph_note_map = fetch_notes(conn, graph_note_ids)

    for note_id in graph_note_ids:
        note = graph_note_map.get(note_id)
        if note is None:
            continue

        distance = graph_note_distance[note_id]
        strength = 8.0 / (distance + 1.0)

        add_evidence(
            kind="graph",
            strength=strength,
            graph_distance=distance,
            content=note["body"],
            note_id=note_id,
            note_title=note["title"],
            note_path=note["path"],
        )

    # ---------------------------------------------------------
    # Evidence tiers
    # ---------------------------------------------------------

    for item in evidence:
        if item["strength"] >= 10:
            item["tier"] = "primary"
        elif item["strength"] >= 5:
            item["tier"] = "context"
        else:
            item["tier"] = "background"

    evidence.sort(
        key=lambda item: (
            item["strength"],
            item["occurred_at"] or "",
        ),
        reverse=True,
    )

    # Keep more raw evidence available for debugging, but prevent unbounded output.
    evidence = evidence[: max(limit * 6, 30)]

    # ---------------------------------------------------------
    # Related entities from graph neighborhood
    # ---------------------------------------------------------

    related_entities = [
        {
            "id": node["id"],
            "name": node["name"],
            "type": node["type"],
            "depth": node["depth"],
        }
        for node in graph.get("nodes", [])
        if not entity or node["id"] != entity["id"]
    ]

    # ---------------------------------------------------------
    # Source note metadata
    # ---------------------------------------------------------

    source_note_ids = {
        item["note_id"]
        for item in evidence
        if item["note_id"] is not None
    }
    source_map = fetch_notes(conn, source_note_ids)

    source_notes = [
        {
            "note_id": note_id,
            "title": row["title"],
            "path": row["path"],
            "type": row["type"],
        }
        for note_id, row in sorted(
            source_map.items(),
            key=lambda item: item[1]["path"],
        )
    ]

    return {
        "query": query,
        "period": {
            "start": start,
            "end": end,
        },
        "entity": entity_info(entity),
        "graph_depth": graph_depth,
        "graph": graph,
        "entities": (
            [entity_info(entity)]
            if entity is not None
            else []
        ),
        "related_entities": related_entities,
        "events": [
            {
                "id": item["row"]["id"],
                "occurred_at": item["row"]["occurred_at"],
                "event_type": item["row"]["event_type"],
                "description": item["row"]["description"],
                "entity": item["entity"],
                "note_id": item["row"]["note_id"],
                "note_title": item["row"]["note_title"],
                "note_path": item["row"]["note_path"],
                "graph_distance": item["graph_distance"],
            }
            for item in sorted(
                event_best.values(),
                key=lambda value: value["row"]["occurred_at"],
                reverse=True,
            )[:limit]
        ],
        "states": [
            {
                "state": item["row"]["state"],
                "valid_from": item["row"]["valid_from"],
                "valid_to": item["row"]["valid_to"],
                "entity": item["row"]["entity"],
                "note_title": item["row"]["note_title"],
                "note_path": item["row"]["note_path"],
                "graph_distance": item["graph_distance"],
            }
            for item in sorted(
                state_best.values(),
                key=lambda value: value["row"]["valid_from"],
                reverse=True,
            )[:limit]
        ],
        "evidence": {
            "primary": [
                item for item in evidence if item["tier"] == "primary"
            ][:limit],
            "context": [
                item for item in evidence if item["tier"] == "context"
            ][:limit],
            "background": [
                item for item in evidence if item["tier"] == "background"
            ][:limit],
        },
        "source_notes": source_notes,
    }


# ---------------------------------------------------------------------------
# LLM-facing context compaction
# ---------------------------------------------------------------------------


def compact_context(
    result: dict[str, Any],
    max_chars: int = 7000,
) -> dict[str, Any]:
    """Merge duplicate evidence and produce a bounded LLM-facing context."""

    if max_chars < 1000:
        raise ValueError("max_chars must be at least 1000.")

    query = result.get("query", "")
    entity = result.get("entity")
    graph = result.get("graph", {})
    source_lookup = {
        note["note_id"]: note
        for note in result.get("source_notes", [])
    }

    tier_rank = {
        "primary": 3,
        "context": 2,
        "background": 1,
    }

    query_terms = [
        token.lower()
        for token in search_tokens(query)
        if len(token) >= 3
    ]

    def excerpt(text: str | None, max_length: int) -> str:
        text = (text or "").strip()
        if len(text) <= max_length:
            return text

        lower_text = text.lower()
        positions = [
            lower_text.find(term)
            for term in query_terms
            if lower_text.find(term) >= 0
        ]

        start = (
            max(0, min(positions) - max_length // 3)
            if positions
            else 0
        )
        end = min(len(text), start + max_length)

        prefix = "[...] " if start else ""
        suffix = " [...]" if end < len(text) else ""

        return prefix + text[start:end].rstrip() + suffix

    # ---------------------------------------------------------
    # Merge all evidence belonging to the same source note.
    # ---------------------------------------------------------

    merged: dict[Any, dict[str, Any]] = {}

    for tier_name in ("primary", "context", "background"):
        for item in result.get("evidence", {}).get(tier_name, []):
            note_id = item.get("note_id")

            # Evidence without a source note is retained separately instead
            # of colliding with another source-less item.
            key: Any = (
                ("note", note_id)
                if note_id is not None
                else (
                    "evidence",
                    item.get("kind"),
                    item.get("content", ""),
                )
            )

            memory = merged.setdefault(
                key,
                {
                    "note_id": note_id,
                    "title": item.get("note_title"),
                    "path": item.get("note_path"),
                    "best_tier": tier_name,
                    "best_strength": 0.0,
                    "graph_distance": None,
                    "reasons": set(),
                    "events": [],
                    "states": [],
                    "note_context": "",
                },
            )

            if tier_rank[tier_name] > tier_rank[memory["best_tier"]]:
                memory["best_tier"] = tier_name

            memory["best_strength"] = max(
                memory["best_strength"],
                float(item.get("strength", 0.0)),
            )

            distance = item.get("graph_distance")
            if distance is not None:
                if (
                    memory["graph_distance"] is None
                    or distance < memory["graph_distance"]
                ):
                    memory["graph_distance"] = distance

            memory["reasons"].add(
                item.get("kind", "unknown")
            )

            if item.get("kind") == "event":
                event = {
                    "occurred_at": item.get("occurred_at"),
                    "entity": item.get("entity"),
                    "content": item.get("content", ""),
                }
                if event not in memory["events"]:
                    memory["events"].append(event)

            elif item.get("kind") == "state":
                state = {
                    "valid_from": item.get("occurred_at"),
                    "entity": item.get("entity"),
                    "content": item.get("content", ""),
                }
                if state not in memory["states"]:
                    memory["states"].append(state)

            elif item.get("kind") in {"full_text", "graph"}:
                content = item.get("content", "").strip()
                if len(content) > len(memory["note_context"]):
                    memory["note_context"] = content

    memories = []

    for memory in merged.values():
        source = source_lookup.get(memory["note_id"])

        if source:
            memory["title"] = memory["title"] or source["title"]
            memory["path"] = memory["path"] or source["path"]

        memories.append(
            {
                "note_id": memory["note_id"],
                "title": memory["title"],
                "path": memory["path"],
                "relevance": memory["best_tier"],
                "strength": round(memory["best_strength"], 3),
                "graph_distance": memory["graph_distance"],
                "reasons": sorted(memory["reasons"]),
                "events": [
                    {
                        "occurred_at": event["occurred_at"],
                        "entity": event["entity"],
                        "content": excerpt(event["content"], 500),
                    }
                    for event in sorted(
                        memory["events"],
                        key=lambda item: item["occurred_at"] or "",
                    )
                ],
                "states": [
                    {
                        "valid_from": state["valid_from"],
                        "entity": state["entity"],
                        "content": excerpt(state["content"], 400),
                    }
                    for state in sorted(
                        memory["states"],
                        key=lambda item: item["valid_from"] or "",
                    )
                ],
                "note_context": excerpt(
                    memory["note_context"],
                    600,
                ),
            }
        )

    memories.sort(
        key=lambda item: (
            tier_rank.get(item["relevance"], 0),
            item["strength"],
            -(
                item["graph_distance"]
                if item["graph_distance"] is not None
                else 999
            ),
        ),
        reverse=True,
    )

    # ---------------------------------------------------------
    # Graph: keep a consistent depth boundary.
    # ---------------------------------------------------------

    compact_depth = min(
        int(result.get("graph_depth", 2)),
        2,
    )

    graph_nodes = [
        {
            "name": node["name"],
            "type": node["type"],
            "depth": node["depth"],
        }
        for node in graph.get("nodes", [])
        if node.get("depth", 0) <= compact_depth
    ]

    visible_names = {
        node["name"]
        for node in graph_nodes
    }

    graph_relationships = [
        {
            "source": relationship["source"],
            "relationship": relationship["relationship"],
            "target": relationship["target"],
        }
        for relationship in graph.get("relationships", [])
        if (
            relationship["source"] in visible_names
            and relationship["target"] in visible_names
        )
    ]

    # ---------------------------------------------------------
    # Source-note metadata only. Full bodies live in memories.
    # ---------------------------------------------------------

    used_note_ids = {
        memory["note_id"]
        for memory in memories
        if memory["note_id"] is not None
    }

    source_notes = [
        note
        for note in result.get("source_notes", [])
        if note["note_id"] in used_note_ids
    ]
    source_notes.sort(key=lambda note: note["path"])

    compact = {
        "query": query,
        "period": result.get("period", {}),
        "focus_entity": entity,
        "graph": {
            "nodes": graph_nodes,
            "relationships": graph_relationships,
        },
        "memories": memories,
        "source_notes": source_notes,
    }

    def serialized_size(data: dict[str, Any]) -> int:
        return len(
            json.dumps(
                data,
                ensure_ascii=False,
                separators=(",", ":"),
            )
        )

    # Lower-value memories go first; remove them until the budget is met.
    while serialized_size(compact) > max_chars:
        background = [
            i
            for i, memory in enumerate(compact["memories"])
            if memory["relevance"] == "background"
        ]
        if background:
            compact["memories"].pop(background[-1])
            continue

        contextual = [
            i
            for i, memory in enumerate(compact["memories"])
            if memory["relevance"] == "context"
        ]
        if contextual:
            compact["memories"].pop(contextual[-1])
            continue

        # Preserve primary structured evidence; trim prose next.
        changed = False
        for memory in compact["memories"]:
            if memory["note_context"]:
                memory["note_context"] = ""
                changed = True
                break
        if changed:
            continue

        # Then trim graph detail.
        if compact["graph"]["relationships"]:
            compact["graph"]["relationships"].pop()
            continue

        if len(compact["graph"]["nodes"]) > 1:
            compact["graph"]["nodes"].pop()
            continue

        break

    compact["context_chars"] = serialized_size(compact)
    return compact


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Local second-brain retrieval engine"
    )

    subparsers = parser.add_subparsers(
        dest="command",
        required=True,
    )

    think = subparsers.add_parser(
        "think",
        help="Retrieve thoughts and decisions about an entity",
    )
    think.add_argument("entity")
    think.add_argument("--limit", type=int, default=20)

    timeline = subparsers.add_parser(
        "timeline",
        help="Retrieve all recorded events for an entity",
    )
    timeline.add_argument("entity")
    timeline.add_argument("--limit", type=int, default=50)

    state = subparsers.add_parser(
        "state",
        help="Retrieve the state of an entity",
    )
    state.add_argument("entity")
    state.add_argument("--as-of", default=None)

    related = subparsers.add_parser(
        "related",
        help="Retrieve directly connected entities and notes",
    )
    related.add_argument("entity")
    related.add_argument("--limit", type=int, default=20)

    search = subparsers.add_parser(
        "search",
        help="Full-text search Markdown notes",
    )
    search.add_argument("query")
    search.add_argument("--limit", type=int, default=20)

    context = subparsers.add_parser(
        "context",
        help="Build entity-centric context",
    )
    context.add_argument("entity")
    context.add_argument("--limit", type=int, default=10)

    time_parser = subparsers.add_parser(
        "time",
        help="Retrieve events and states for a time period",
    )
    time_parser.add_argument(
        "start",
        help="Start date in YYYY-MM-DD format",
    )
    time_parser.add_argument(
        "end",
        nargs="?",
        default=None,
        help="Inclusive end date in YYYY-MM-DD format",
    )
    time_parser.add_argument("--limit", type=int, default=50)

    query = subparsers.add_parser(
        "query",
        help="Hybrid text, graph, and temporal retrieval",
    )
    query.add_argument(
        "query",
        help="Topic, entity, or search query",
    )
    query.add_argument(
        "--from",
        dest="start",
        default=None,
        help="Start date in YYYY-MM-DD format",
    )
    query.add_argument(
        "--to",
        dest="end",
        default=None,
        help="Inclusive end date in YYYY-MM-DD format",
    )
    query.add_argument("--limit", type=int, default=10)
    query.add_argument(
        "--depth",
        type=int,
        default=2,
        help="Maximum graph traversal depth",
    )
    query.add_argument(
        "--compact",
        action="store_true",
        help="Return compact LLM-ready context",
    )
    query.add_argument(
        "--max-chars",
        type=int,
        default=7000,
        help="Maximum compact context size",
    )

    graph = subparsers.add_parser(
        "graph",
        help="Traverse the knowledge graph",
    )
    graph.add_argument("entity")
    graph.add_argument("--depth", type=int, default=2)
    graph.add_argument("--limit", type=int, default=50)

    return parser


def main() -> None:
    args = build_parser().parse_args()
    conn = connect()

    try:
        if args.command == "think":
            result = get_thoughts(conn, args.entity, args.limit)
        elif args.command == "timeline":
            result = get_timeline(conn, args.entity, args.limit)
        elif args.command == "state":
            result = get_state(conn, args.entity, args.as_of)
        elif args.command == "related":
            result = get_related(conn, args.entity, args.limit)
        elif args.command == "search":
            result = search_notes(conn, args.query, args.limit)
        elif args.command == "context":
            result = get_context(conn, args.entity, args.limit)
        elif args.command == "time":
            result = get_time_context(
                conn,
                args.start,
                args.end,
                args.limit,
            )
        elif args.command == "query":
            result = get_hybrid_context(
                conn,
                args.query,
                args.start,
                args.end,
                args.limit,
                args.depth,
            )
            if args.compact:
                result = compact_context(
                    result,
                    args.max_chars,
                )
        elif args.command == "graph":
            result = get_graph(
                conn,
                args.entity,
                args.depth,
                args.limit,
            )
        else:
            raise RuntimeError(
                f"Unknown command: {args.command}"
            )

        print(
            json.dumps(
                result,
                indent=2,
                ensure_ascii=False,
            )
        )
    finally:
        conn.close()


if __name__ == "__main__":
    main()
