<h1 align="center">Janus</h1>

<div align="center">
<a href=https://en.wikipedia.org/wiki/Janus><img src="./assets/janus.jpg" align="center"></img></a>
  
[![Rust](https://img.shields.io/github/languages/top/AP3008/Janus)](https://github.com/AP3008/Janus)
[![Stars](https://img.shields.io/github/stars/AP3008/Janus)](https://github.com/AP3008/Janus/stargazers)
[![Last Commit](https://img.shields.io/github/last-commit/AP3008/Janus)](https://github.com/AP3008/Janus/commits)
[![License](https://img.shields.io/github/license/AP3008/Janus)](LICENSE)

<blockquote>
        <p><i>'Presiding over all beginnings and transitions, whether abstract or concrete, sacred or profane.'</i></p>
</blockquote>

An LLM token compression proxy for the Anthropic API. Janus sits between your application and Claude, intelligently compressing requests to reduce token usage and cost without sacrificing context quality.

**1x GenAI Genesis Winner: 🏆 Google Sustainability Hack**

</div>

## Inspiration

I wanted to build something that runs locally, losslessly, and efficiently that significantly decreases the token usage, to maximize utility out of coding agents. 

## What It Does

Janus intercepts outgoing API requests to Anthropic's `/v1/messages` endpoint and runs them through a multi-stage compression pipeline before forwarding them upstream. Responses are returned transparently to the client, with both streaming and non-streaming modes supported.

### Compression Pipeline

Requests pass through four stages, each targeting a different source of redundancy:

**Stage A -- Tool-Result Deduplication**
Tracks tool call outputs within a conversation session. When the same tool produces identical output more than once, subsequent occurrences are replaced with a short placeholder, eliminating repeated content.

**Stage B -- Regex Structural Compression**
Five sub-stages of pattern-based compression:

- B1: Docstring removal (Python, JSDoc, Rust doc comments)
- B2: Comment stripping
- B3: Whitespace normalization
- B4: Stack trace condensation
- B5: Repeated block deduplication

**Stage C -- AST Pruning**
Uses tree-sitter to parse code blocks (Python, JavaScript, Rust, Go) and remove functions that are unlikely to be relevant to the current query. Only applied to blocks above a configurable line threshold.

### Semantic Cache

On top of the compression pipeline, Janus maintains a semantic cache backed by Redis with vector similarity search. Requests that are semantically similar to previously seen requests (above a configurable similarity threshold) return cached responses directly, skipping the upstream call entirely.

- Embeddings generated locally using BGE-small-en-v1.5 (384-dimensional) via fastembed
- Configurable similarity cutoff (default: 0.85) and TTL (default: 1 hour)

## Architecture

```text
Client --> Janus Proxy (localhost:8080) --> Anthropic API
               |
               |-- Compression Pipeline (Stages A-D)
               |-- Semantic Cache (Redis + Vector Search)
               |-- TUI Dashboard (real-time metrics)
```

## Tech Stack

| Component | Technology |
| --- | --- |
| Language | Rust |
| Async Runtime | Tokio |
| HTTP Framework | Axum |
| Terminal UI | Ratatui + Crossterm |
| AST Parsing | tree-sitter (Python, JS, Rust, Go) |
| Embeddings | fastembed (BGE-small-en-v1.5) |
| Cache | Redis with RediSearch |
| Token Counting | tiktoken-rs |
| Hashing | xxhash (xxh3) |
| Containerization | Docker + Docker Compose |

## Getting Started

### Prerequisites

- Rust toolchain (1.75+)
- Redis server (with RediSearch module for semantic caching)

### Build

```bash
cargo build --release
```

### Configure

Copy and edit the default configuration file:

```bash
cp janus.toml janus.toml.local
```

Key settings in `janus.toml`:

```toml
[server]
listen = "0.0.0.0:8080"
upstream_url = "https://api.anthropic.com"

[pipeline]
tool_dedup = true
regex_structural = true
ast_pruning = true
semantic_trim = true

[cache]
enabled = true
redis_url = "redis://127.0.0.1:6379"
similarity_cutoff = 0.85
ttl_seconds = 3600

[pricing]
input_cost_per_1k = 0.003
output_cost_per_1k = 0.015
```

### Run

```bash
# Start the proxy with the interactive TUI
janus serve

# Start without the TUI (logs to stdout)
janus serve --no-tui

# Use a custom config file
janus serve --config path/to/config.toml
```

### Docker

```bash
# Start Janus + Redis stack
docker-compose up

# Health check
curl http://localhost:8080/health
```

### Other Commands

```bash
# Run compression benchmarks
janus benchmark

# Cache management
janus cache flush
janus cache stats
janus cache test
```

## TUI Dashboard

When running with `janus serve`, an interactive terminal dashboard displays real-time metrics:

- Total tokens saved and estimated cost reduction
- Per-stage compression breakdown
- Request history with cache hit/miss indicators
- Error tracking with timestamps

Keyboard controls: `q` quit, `p` pause, `r` reset stats, `f` flush cache, `a` toggle auto-flush, arrow keys to scroll.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for details.
