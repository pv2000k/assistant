---
title: Local LLM Model Decision
type: journal
tags:
  - ai
  - qwen
  - llama-cpp

entities:
  - name: Local AI
    type: topic
  - name: Qwen3.5
    type: technology
  - name: llama.cpp
    type: software
  - name: Personal Assistant
    type: project

events:
  - type: decision
    occurred_at: 2026-09-03T19:00
    description: The 27B model is impractical on the laptop, so Qwen3.5 4B is being selected as the everyday local model.
    entities:
      - Local AI
      - Qwen3.5
      - Personal Assistant

  - type: test
    occurred_at: 2026-09-03T20:00
    description: llama.cpp successfully detected CUDA on the GTX 1650.
    entities:
      - llama.cpp
      - Qwen3.5

states:
  - entity: Qwen3.5
    state: Selected for evaluation as the everyday assistant
    valid_from: 2026-09-03

relationships:
  - source: Personal Assistant
    target: Qwen3.5
    relationship: uses

  - source: Qwen3.5
    target: llama.cpp
    relationship: runs_on

  - source: Local AI
    target: Qwen3.5
    relationship: evaluates
---

# Local LLM Model Decision

The larger 27B experiment was clearly unreasonable for this hardware.
The current direction is a small Qwen model running locally through
llama.cpp with CUDA acceleration.

The goal is not maximum intelligence. The goal is a useful local
interface that can rely on retrieval rather than holding the entire
second brain in its context.
