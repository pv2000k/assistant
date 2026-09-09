---
title: Qwen3.5 Performance Test
type: journal
tags:
  - ai
  - qwen
  - performance

entities:
  - name: Qwen3.5
    type: technology
  - name: NVIDIA GTX 1650
    type: hardware
  - name: Personal Assistant
    type: project

events:
  - type: measurement
    occurred_at: 2026-09-04T11:10
    description: Qwen3.5 generated around 4.4 tokens per second and used roughly 1.9 GiB of VRAM during the test.
    entities:
      - Qwen3.5
      - NVIDIA GTX 1650

  - type: observation
    occurred_at: 2026-09-04T11:15
    description: Thinking mode caused substantial CPU activity, so the small model may need selective use of reasoning.
    entities:
      - Qwen3.5
      - Personal Assistant

states:
  - entity: Qwen3.5
    state: Viable for lightweight local interaction but reasoning should be used selectively
    valid_from: 2026-09-04

relationships:
  - source: Qwen3.5
    target: NVIDIA GTX 1650
    relationship: runs_on

  - source: Qwen3.5
    target: Personal Assistant
    relationship: candidate_for
---

# Qwen3.5 Performance Test

The model is substantially faster than the previous 27B experiment.
The hardware is still constrained, so retrieval should do most of the
memory work and the LLM should receive compact context.

The first performance test was encouraging enough to keep Qwen3.5 in
consideration.
