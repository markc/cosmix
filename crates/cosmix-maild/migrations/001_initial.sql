-- cosmix-jmap initial schema

CREATE TABLE IF NOT EXISTS accounts (
    id          SERIAL PRIMARY KEY,
    email       TEXT UNIQUE NOT NULL,
    password    TEXT NOT NULL,
    name        TEXT,
    quota       BIGINT DEFAULT 0,
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS mailboxes (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    parent_id   UUID REFERENCES mailboxes(id),
    role        TEXT,
    sort_order  INT DEFAULT 0,
    UNIQUE(account_id, parent_id, name)
);

CREATE TABLE IF NOT EXISTS threads (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS blobs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    size        INT NOT NULL,
    hash        TEXT NOT NULL,
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS emails (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id   UUID NOT NULL REFERENCES threads(id),
    mailbox_ids UUID[] NOT NULL,
    blob_id     UUID NOT NULL REFERENCES blobs(id),
    size        INT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    message_id  TEXT,
    in_reply_to TEXT[],
    subject     TEXT,
    from_addr   JSONB,
    to_addr     JSONB,
    cc_addr     JSONB,
    date        TIMESTAMPTZ,
    preview     TEXT,
    has_attachment BOOLEAN DEFAULT FALSE,
    keywords    JSONB DEFAULT '{}',
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_emails_account_mailbox ON emails USING GIN (mailbox_ids);
CREATE INDEX IF NOT EXISTS idx_emails_thread ON emails (thread_id);
CREATE INDEX IF NOT EXISTS idx_emails_received ON emails (account_id, received_at DESC);
CREATE INDEX IF NOT EXISTS idx_emails_message_id ON emails (message_id);

CREATE TABLE IF NOT EXISTS changelog (
    id          BIGSERIAL PRIMARY KEY,
    account_id  INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    object_type TEXT NOT NULL,
    object_id   UUID NOT NULL,
    change_type TEXT NOT NULL,
    changed_at  TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_changelog_account_type ON changelog (account_id, object_type, id);

-- Default mailboxes function
CREATE OR REPLACE FUNCTION create_default_mailboxes(acct_id INT) RETURNS VOID AS $$
BEGIN
    INSERT INTO mailboxes (account_id, name, role) VALUES
        (acct_id, 'Inbox',    'inbox'),
        (acct_id, 'Drafts',   'drafts'),
        (acct_id, 'Sent',     'sent'),
        (acct_id, 'Trash',    'trash'),
        (acct_id, 'Junk',     'junk'),
        (acct_id, 'Archive',  'archive');
END;
$$ LANGUAGE plpgsql;
