---
title: Temporal Graph Test
type: project
tags:
  - test
  - temporal
  - graph

entities:
  - name: Graph Experiment
    type: project
  - name: Research
    type: topic
  - name: Prototype
    type: topic
  - name: Final System
    type: topic

events:
  - type: decision
    occurred_at: 2026-08-20T10:00
    description: The project is starting as a research exercise.
    entities:
      - Graph Experiment
      - Research

  - type: transition
    occurred_at: 2026-08-25T10:00
    description: The research phase is complete and a prototype phase begins.
    entities:
      - Graph Experiment
      - Prototype

  - type: decision
    occurred_at: 2026-09-04T10:00
    description: The prototype is considered successful enough to become the final system.
    entities:
      - Graph Experiment
      - Final System

states:
  - entity: Graph Experiment
    state: Research
    valid_from: 2026-08-20
    valid_to: 2026-08-24

  - entity: Graph Experiment
    state: Prototype
    valid_from: 2026-08-25
    valid_to: 2026-09-03

  - entity: Graph Experiment
    state: Final System
    valid_from: 2026-09-04

relationships:
  - source: Graph Experiment
    target: Research
    relationship: starts_as

  - source: Graph Experiment
    target: Prototype
    relationship: evolves_into

  - source: Prototype
    target: Final System
    relationship: evolves_into
---

# Temporal Graph Test

This is synthetic test data for the second-brain retrieval system.

The project evolves from research to prototype to final system.
