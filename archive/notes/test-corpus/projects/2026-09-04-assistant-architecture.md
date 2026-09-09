---
title: Second Brain Architecture
type: project
tags:
  - assistant
  - architecture
  - memory

entities:
  - name: Personal Assistant
    type: project
  - name: Markdown
    type: technology
  - name: SQLite
    type: technology
  - name: Temporal Graph
    type: concept
  - name: Retrieval Engine
    type: software

events:
  - type: decision
    occurred_at: 2026-09-04T12:00
    description: Markdown is the permanent source of truth for the second brain.
    entities:
      - Personal Assistant
      - Markdown

  - type: design
    occurred_at: 2026-09-04T13:00
    description: SQLite will index full text, entities, relationships, temporal events, and states so the small LLM receives only relevant context.
    entities:
      - Personal Assistant
      - SQLite
      - Temporal Graph
      - Retrieval Engine

relationships:
  - source: Personal Assistant
    target: Markdown
    relationship: stores_memory_in

  - source: Personal Assistant
    target: SQLite
    relationship: indexes_with

  - source: Personal Assistant
    target: Temporal Graph
    relationship: uses

  - source: Temporal Graph
    target: Retrieval Engine
    relationship: feeds
---

# Second Brain Architecture

The second brain should accept ideas, thoughts, events, facts, projects,
people, tasks, and reminders without requiring perfect organization.

Markdown remains the source of truth. SQLite provides machine-readable
structure and retrieval.

The temporal graph should eventually connect topics, subtopics,
projects, people, events, and changing states across branches.
