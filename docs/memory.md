# Memory / RAG System

MyLobster includes an embedded memory system built on SQLite with full-text search (FTS5) and vector-based semantic search, combined via Reciprocal Rank Fusion (RRF) for hybrid retrieval.

## Architecture

```
Agent query
    │
    ▼
┌──────────────────────┐
│  MemoryIndexManager   │  ← Per-agent SQLite database
│  (src/memory/manager) │
└──────┬───────┬───────┘
       │       │
       ▼       ▼
   ┌───────┐ ┌────────┐
   │ FTS5  │ │ Vector │
   │ BM25  │ │ cosine │
   └───┬───┘ └───┬────┘
       │         │
       ▼         ▼
   ┌───────────────────┐
   │  Hybrid RRF Merge  │  ← Reciprocal Rank Fusion
   │  (src/memory/hybrid)│
   └────────────────────┘
```

## Components

### MemoryIndexManager (`src/memory/manager.rs`)

The main orchestrator. Each agent gets its own SQLite database at `<state_dir>/memory/<agent_id>.db`.

```rust
let manager = MemoryIndexManager::get(&config, "agent-1")?;
let results = manager.search("rust async patterns", &opts).await?;
manager.sync(&sync_params).await?;  // Index files from filesystem
manager.close()?;
```

**Methods:**

| Method | Description |
|--------|-------------|
| `get(config, agent_id)` | Open or create the agent's memory database |
| `search(query, opts)` | Execute hybrid BM25 + vector search |
| `sync(params)` | Index files from the filesystem into the memory store |
| `status()` | Returns `Ready`, `Syncing`, `Closed`, or `Error` |
| `close()` | Release the database connection |

### SQLite Schema (`src/memory/schema.rs`)

Uses WAL mode for concurrent reads. Current schema version: v1.

| Table | Purpose |
|-------|---------|
| `meta` | Schema version and key-value metadata |
| `files` | Tracked files (path, content hash, modification time, chunk count) |
| `chunks` | Text chunks with embedding BLOBs (linked to files, with line ranges) |
| `embedding_cache` | Caches embeddings by content hash to avoid redundant API calls |
| `chunks_fts` | FTS5 virtual table for BM25 full-text search |

The `chunks_fts` table has triggers that auto-sync with the `chunks` table on insert/update/delete.

### Search (`src/memory/search.rs`)

Three search modes:

| Mode | Engine | Use case |
|------|--------|----------|
| `Fts` | SQLite FTS5 (BM25) | Exact keyword matching |
| `Vector` | Cosine similarity on embeddings | Semantic / conceptual matching |
| `Hybrid` | FTS + Vector merged via RRF | Best of both (default) |

**Search options:**

```rust
MemorySearchOptions {
    max_results: usize,   // default: 10
    min_score: f64,       // minimum relevance threshold (0.0-1.0)
    session_key: Option<String>,
    mode: SearchMode,     // Fts, Vector, or Hybrid
}
```

**Result format:**

```rust
MemorySearchResult {
    text: String,         // The matched text chunk
    path: String,         // Source file path
    score: f64,           // Relevance score (0.0-1.0, normalized)
    start_line: usize,    // Start line in source file
    end_line: usize,      // End line in source file
}
```

### Hybrid Scoring (`src/memory/hybrid.rs`)

Combines FTS and vector results using Reciprocal Rank Fusion (RRF):

```
score = weight / (k + rank)
```

Where `k = 60` (standard RRF constant). Results are de-duplicated by chunk ID, and final scores are min-max normalized to the `[0, 1]` range.

### Embeddings (`src/memory/embeddings.rs`)

Four embedding providers, selected by configuration:

| Provider | Model | Dimensions | Env Var |
|----------|-------|------------|---------|
| OpenAI | `text-embedding-3-small` | 1536 | `OPENAI_API_KEY` |
| Gemini | `text-embedding-004` | 768 | `GOOGLE_API_KEY` |
| Voyage | `voyage-3` | 1024 | `VOYAGE_API_KEY` |
| Local | placeholder | configurable | — |

All providers implement the `EmbeddingProvider` trait:

```rust
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f64>>>;
    fn model_name(&self) -> &str;
    fn dimensions(&self) -> usize;
}
```

### Text Chunking (`src/memory/chunking.rs`)

Files are split into overlapping chunks before embedding:

```rust
let chunks = chunk_text(content, max_tokens, overlap);
// Each chunk includes: text, start_line, end_line, token_count
```

Tokenization is whitespace-based. Chunks preserve line number ranges so search results can reference exact file locations.

## Configuration

```json
{
  "memory": {
    "embedding": {
      "provider": "openai",
      "model": "text-embedding-3-small"
    },
    "search": {
      "mode": "hybrid",
      "maxResults": 10,
      "minScore": 0.3
    },
    "sync": {
      "paths": ["./docs", "./src"],
      "include": ["*.md", "*.rs", "*.ts"],
      "exclude": ["node_modules", "target"]
    },
    "chunking": {
      "maxTokens": 512,
      "overlap": 64
    }
  }
}
```
