-- SMTP outbound queue
CREATE TABLE IF NOT EXISTS smtp_queue (
    id          BIGSERIAL PRIMARY KEY,
    from_addr   TEXT NOT NULL,
    to_addrs    TEXT[] NOT NULL,
    blob_id     UUID REFERENCES blobs(id),
    attempts    INT DEFAULT 0,
    next_retry  TIMESTAMPTZ DEFAULT NOW(),
    last_error  TEXT,
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_queue_retry ON smtp_queue (next_retry) WHERE attempts < 10;
