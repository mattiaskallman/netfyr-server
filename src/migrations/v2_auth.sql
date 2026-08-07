-- =====================================================================
-- migrations/v2_auth.sql
-- Etapp 5: autentisering, roller och auditlogg.
--
-- Körs som migrationssteg 1 → 2 i db.rs. Idempotent (IF NOT EXISTS).
-- =====================================================================

-- ---- Användare -------------------------------------------------------
--
-- Lösenorden lagras som Argon2id-hashar. Det enda personuppgiftsliknande
-- som finns är användarnamnet — ingen e-post, inget namn. Det är
-- avsiktlig dataminimering: det som inte lagras kan inte läcka.

CREATE TABLE IF NOT EXISTS users (
    id                   INTEGER PRIMARY KEY,
    username             TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash        TEXT NOT NULL,
    role                 TEXT NOT NULL CHECK(role IN ('admin','user')),

    -- Engångslösen vid skapande/återställning tvingar byte vid första
    -- inloggningen. Annars skulle det tillfälliga lösenordet leva kvar.
    must_change_password INTEGER NOT NULL DEFAULT 0,

    -- Avstängt konto: kan inte logga in, sessioner rensas. Kvar i
    -- auditloggen — ett raderat konto får inte sudda sitt spår.
    disabled             INTEGER NOT NULL DEFAULT 0,

    -- Utelåsning mot lösenordsgissning. Fem misslyckade försök i rad
    -- låser kontot i en kvart. Räknaren nollas vid lyckad inloggning.
    failed_attempts      INTEGER NOT NULL DEFAULT 0,
    locked_until         INTEGER,

    last_login_at        INTEGER,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL
);

-- ---- Sessioner -------------------------------------------------------
--
-- Kakans värde är 32 slumpade byte, hexkodade. Här lagras bara SHA-256
-- av värdet: en stulen databasdump ger inga användbara token.
--
-- Utgångna sessioner städas av städloopen (auth.rs), men även en
-- ogiltig session fångas av expires_at-kollen vid varje anrop.

CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    ip         TEXT,
    user_agent TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_expiry ON sessions(expires_at);

-- ---- Auditlogg -------------------------------------------------------
--
-- Vem gjorde vad, när, varifrån. Skild från events: events beskriver
-- nätet, audit_log beskriver användarna. Krav i NIS2:s spårbarhets-
-- och incidentarbete — utan den går ett intrång inte att rekonstruera.
--
-- Ingen redigering, ingen radering via API:t. En logg som går att
-- ändra är inte en logg. Gallring sker av städloopen efter
-- AUDIT_RETENTION_DAYS dygn.

CREATE TABLE IF NOT EXISTS audit_log (
    id       INTEGER PRIMARY KEY,
    ts       INTEGER NOT NULL,
    username TEXT NOT NULL,
    action   TEXT NOT NULL,          -- login_ok | login_fail | host_create | ...
    target   TEXT,                   -- vad som berördes: enhet, grupp, användare
    detail   TEXT,                   -- kort förklaring, aldrig hemligheter
    ip       TEXT
);

CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log(ts);
