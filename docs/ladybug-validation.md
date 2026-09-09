# Ladybug Validation

## Environment

- OS: NixOS
- Python: 3.13.15
- Rust: 1.95.0
- Cargo: 1.95.0
- Rust Ladybug binding: lbug 0.20.3
- Nix Ladybug CLI tested: 0.15.3
- GPU: NVIDIA GTX 1650, 4 GiB VRAM

## Proven

### Rust + Ladybug core
- Database creation works.
- Node tables work.
- Node insertion works.
- Cypher queries work.
- Relationship tables work.
- Graph traversal works.

### FTS
- Ladybug CLI FTS works.
- BM25 retrieval works.
- Rust FTS initially failed because dynamically loaded extensions could not resolve a Ladybug symbol.
- Adding `-rdynamic` to the Rust executable fixed the issue.
- Rust + lbug 0.20.3 + FTS works.

### Vector
- Vector extension works.
- FLOAT vectors work.
- Cosine similarity works.
- HNSW vector index works.
- Vector search works through the CLI.
- Vector search works through Rust.
- Vector retrieval correctly ranks semantically close vectors.

### Graph + Vector
- Vector search followed by graph traversal works.
- Graph constrained vector retrieval works using a projected graph.

### Temporal
- TIMESTAMP properties work.
- Temporal filtering works with vector results.

### Combined retrieval primitive

The following pattern has been proven:

semantic similarity
    +
graph relationship
    +
temporal filtering
    =
relevant memory candidates

## Important architecture conclusions

- Ladybug is currently the preferred backend candidate.
- Rust is the preferred application language.
- Markdown remains the source of truth.
- Database/indexes are derived and rebuildable.
- Qwen3.5 4B is the current local everyday model.
- Embedding model has not yet been selected.
- Model router is deferred until the final phase.
- Asynchronous embedding is a candidate.
- Background jobs must obey CPU/RAM/GPU/concurrency budgets.
