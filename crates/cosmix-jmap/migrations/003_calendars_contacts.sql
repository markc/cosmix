-- Calendars (JMAP Calendar objects)
CREATE TABLE IF NOT EXISTS calendars (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id      INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    color           TEXT,
    description     TEXT,
    is_visible      BOOLEAN DEFAULT TRUE,
    default_alerts  JSONB,
    timezone        TEXT DEFAULT 'UTC',
    sort_order      INT DEFAULT 0,
    UNIQUE(account_id, name)
);

-- Calendar events (JSCalendar format — RFC 8984)
CREATE TABLE IF NOT EXISTS calendar_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    calendar_id     UUID NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    account_id      INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    uid             TEXT NOT NULL,
    data            JSONB NOT NULL,
    -- Denormalized for queries
    title           TEXT,
    start_dt        TIMESTAMPTZ,
    end_dt          TIMESTAMPTZ,
    updated_at      TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(calendar_id, uid)
);
CREATE INDEX IF NOT EXISTS idx_events_account ON calendar_events (account_id);
CREATE INDEX IF NOT EXISTS idx_events_range ON calendar_events (calendar_id, start_dt, end_dt);

-- Address books (JMAP AddressBook objects)
CREATE TABLE IF NOT EXISTS addressbooks (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id      INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    description     TEXT,
    sort_order      INT DEFAULT 0,
    UNIQUE(account_id, name)
);

-- Contacts (JSContact format — RFC 9553)
CREATE TABLE IF NOT EXISTS contacts (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    addressbook_id  UUID NOT NULL REFERENCES addressbooks(id) ON DELETE CASCADE,
    account_id      INT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    uid             TEXT NOT NULL,
    data            JSONB NOT NULL,
    -- Denormalized for queries
    full_name       TEXT,
    email           TEXT,
    company         TEXT,
    updated_at      TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(addressbook_id, uid)
);
CREATE INDEX IF NOT EXISTS idx_contacts_account ON contacts (account_id);
CREATE INDEX IF NOT EXISTS idx_contacts_name ON contacts (full_name);

-- Default calendar and addressbook function
CREATE OR REPLACE FUNCTION create_default_pim(acct_id INT) RETURNS VOID AS $$
BEGIN
    INSERT INTO calendars (account_id, name, color) VALUES
        (acct_id, 'Personal', '#4285f4')
    ON CONFLICT DO NOTHING;
    INSERT INTO addressbooks (account_id, name) VALUES
        (acct_id, 'Contacts')
    ON CONFLICT DO NOTHING;
END;
$$ LANGUAGE plpgsql;
