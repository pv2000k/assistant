---
title: Personal Assistant
type: project
tags:
  - ai
  - assistant
  - knowledge-management

entities:
  - name: Personal Assistant
    type: project
  - name: SQLite
    type: technology
  - name: Logseq
    type: software
  - name: Qwen3.5
    type: technology

events:
  - type: thought
    occurred_at: 2026-09-04T11:10
    description: Qwen3.5 4B seems fast enough for everyday use.
    entities:
      - Qwen3.5

  - type: decision
    occurred_at: 2026-09-04T12:00
    description: Use Markdown as the permanent source of truth.

states:
  - entity: Qwen3.5
    state: Evaluating as everyday assistant
    valid_from: 2026-09-04

relationships:
  - source: Personal Assistant
    target: SQLite
    relationship: uses
  - source: Personal Assistant
    target: Logseq
    relationship: uses
---

# Personal Assistant

A local personal assistant built around Markdown, SQLite, Logseq,
Google Calendar, Google Tasks, and a local language model.

Markdown is the source of truth.
