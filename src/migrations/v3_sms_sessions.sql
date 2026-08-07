-- v3: SMS-eskalering och kvittering (etapp 7)
--
-- Sessioner överlever omstarter, precis som leveranskön. En öppen
-- session återupptas av tickern direkt efter uppstart — hellre ett
-- sent eskalerings-SMS än ett larm som tyst dör med processen.

CREATE TABLE IF NOT EXISTS sms_sessions (
    id                  TEXT PRIMARY KEY,        -- kort kod, t.ex. "A7"
    device              TEXT NOT NULL,
    address             TEXT NOT NULL,
    recipients          TEXT NOT NULL,           -- JSON-array
    sent_count          INTEGER NOT NULL DEFAULT 0,
    next_escalation_at  INTEGER,                 -- ms epoch, NULL när stängd
    created_at          INTEGER NOT NULL,        -- ms epoch
    expires_at          INTEGER NOT NULL,        -- ms epoch (kvittensfönster)
    acked_at            INTEGER,
    acked_by            TEXT,
    closed              INTEGER NOT NULL DEFAULT 0,
    closed_reason       TEXT                     -- ack|recovered|expired|exhausted
);

CREATE INDEX IF NOT EXISTS idx_sms_sessions_open
    ON sms_sessions(closed);

-- Behandlade inkorgsmeddelanden: avsändare|datum|textprefix.
-- Gatewayens id är ett löpande index som återanvänds vid radering,
-- så det duger aldrig som avdupliceringsnyckel ensamt.
CREATE TABLE IF NOT EXISTS sms_seen_inbox (
    key      TEXT PRIMARY KEY,
    seen_at  INTEGER NOT NULL
);
