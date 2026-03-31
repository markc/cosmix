-- Cosmix Memory Database Schema
-- Database: cosmix_memory

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS memory_chunks (
    id          BIGSERIAL PRIMARY KEY,
    session_id  TEXT,
    source      TEXT,                        -- 'claude_code', 'openclaw', 'user', 'file'
    content     TEXT NOT NULL,
    summary     TEXT,
    embedding   vector(768),                 -- nomic-embed-text dimensions
    metadata    JSONB DEFAULT '{}',
    created_at  TIMESTAMPTZ DEFAULT NOW(),
    accessed_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS task_log (
    id          BIGSERIAL PRIMARY KEY,
    task        TEXT NOT NULL,
    outcome     TEXT,
    agent       TEXT,                        -- 'local', 'claude_code', 'openclaw'
    tokens_used INTEGER DEFAULT 0,
    duration_ms INTEGER,
    metadata    JSONB DEFAULT '{}',
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS file_index (
    id          BIGSERIAL PRIMARY KEY,
    filepath    TEXT UNIQUE NOT NULL,
    content_hash TEXT,
    summary     TEXT,
    embedding   vector(768),
    last_indexed TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS memory_chunks_embedding_idx
    ON memory_chunks USING ivfflat (embedding vector_cosine_ops)
    WITH (lists = 100);

CREATE INDEX IF NOT EXISTS file_index_embedding_idx
    ON file_index USING ivfflat (embedding vector_cosine_ops)
    WITH (lists = 100);
