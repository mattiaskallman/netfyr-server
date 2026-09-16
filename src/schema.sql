-- =====================================================================
-- schema.sql
-- Databasschema för NetFyr Server.
--
-- Bäddas in i binären med include_str! och körs vid uppstart. Skriptet
-- måste därför vara idempotent: alla satser använder IF NOT EXISTS.
--
-- Versionshantering sker med PRAGMA user_version i db.rs. Framtida
-- ändringar läggs som separata migrationssteg, inte genom att redigera
-- den här filen — annars kan befintliga installationer inte uppgraderas.
-- =====================================================================

-- ---- Inställningar ---------------------------------------------------
--
-- Nyckel/värde för globala inställningar: bevakning på/av, larm på/av,
-- svepintervall, standardtrösklar. Motsvarar det som ligger i
-- localStorage på desktopvarianten.

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ---- Grupper ---------------------------------------------------------

CREATE TABLE IF NOT EXISTS groups (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

-- ---- Enheter ---------------------------------------------------------
--
-- Adressen är unik och används som nyckel mot samples och host_status.
-- Det speglar desktopvarianten, där adressen är enhetens identitet.
--
-- NULL i override-fälten betyder "ärv global inställning". Skillnaden
-- mot 0 är betydelsebärande och får inte slås ihop.

CREATE TABLE IF NOT EXISTS hosts (
    id                 INTEGER PRIMARY KEY,
    name               TEXT NOT NULL,
    address            TEXT NOT NULL UNIQUE,
    group_id           INTEGER REFERENCES groups(id) ON DELETE SET NULL,
    note               TEXT,
    enabled            INTEGER NOT NULL DEFAULT 1,

    -- Override, NULL = ärv globalt
    --
    -- interval_sec är egen svepfrekvens. En kärnswitch kan behöva
    -- kontrolleras var femte sekund medan en skrivare räcker med varje
    -- minut — ett gemensamt intervall tvingar fram det snabbaste för
    -- alla, vilket belastar både nät och databas i onödan.
    interval_sec       INTEGER,
    packet_size        INTEGER,
    slow_threshold_ms  INTEGER,
    fail_period_sec    INTEGER,
    success_period_sec INTEGER,

    -- Beroende: enheten larmar inte om denna adress också är nere.
    --
    -- Adress, inte id: desktopvarianten använder id internt men adress
    -- vid export. Adressen är UNIQUE och överlever både export, import
    -- och id-omnumrering.
    depends_on_address TEXT,

    -- Manuell tystnad till och med denna tidpunkt (ms).
    -- Skild från kvittering: snooze är tidsbegränsad och satt i förväg,
    -- kvittering är ett svar på ett larm som redan gått.
    snooze_until       INTEGER,

    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_hosts_group ON hosts(group_id);

-- ---- Bekräftad status ------------------------------------------------
--
-- Endast den bekräftade statusen persisteras. Den pågående sviten börjar
-- om vid omstart — en svit som påbörjades före en omstart säger inget om
-- läget efteråt.

CREATE TABLE IF NOT EXISTS host_status (
    address        TEXT PRIMARY KEY,
    status         TEXT NOT NULL,      -- unknown | up | down

    -- Senast RAPPORTERADE status, alltså den som ett larm faktiskt gått
    -- ut om. Skiljer sig från status när ett larm hållits tillbaka av
    -- undertryckning eller avslagna larm. Genom att jämföra de två varje
    -- svep levereras larmet så snart hindret upphör.
    reported_status TEXT,

    -- När statusen SENAST FAKTISKT ÄNDRADES. Uppdateras inte vid varje
    -- svep — "nere sedan 14:32" är vad en operatör vill se, inte
    -- tidpunkten för den senaste skrivningen.
    changed_at     INTEGER NOT NULL,

    -- När enheten senast mättes. Visar om uppgifterna är färska, vilket
    -- changed_at inte kan göra när det bara rör sig vid förändring.
    checked_at     INTEGER,

    acked_at       INTEGER,
    acked_by       TEXT,               -- användarnamn, etapp 5

    -- MIKROsekunder, inte millisekunder. Ett lokalt nät svarar på
    -- 0,15 ms, vilket avrundas till noll som heltal ms och ser ut som
    -- att ingen mätning skett. Frontend formaterar för läsbarhet.
    last_latency_us INTEGER
);

-- ---- Mätvärden -------------------------------------------------------
--
-- Historiken sparar högst en oförändrad mätning per enhet och minut,
-- men varje statusväxling direkt. Själva övervakningen kan gå tätare.

CREATE TABLE IF NOT EXISTS samples (
    id         INTEGER PRIMARY KEY,
    address    TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    online     INTEGER NOT NULL,
    -- Mikrosekunder. Se kommentaren i host_status.
    latency_us INTEGER
);

CREATE INDEX IF NOT EXISTS idx_samples_ts      ON samples(ts);
CREATE INDEX IF NOT EXISTS idx_samples_addr_ts ON samples(address, ts);

-- ---- Händelselogg ----------------------------------------------------
--
-- Vad som hände i nätet. Skild från auditloggen (etapp 5), som svarar på
-- vem som gjorde vad.

CREATE TABLE IF NOT EXISTS events (
    id    INTEGER PRIMARY KEY,
    ts    INTEGER NOT NULL,
    level TEXT NOT NULL,               -- debug | info | ok | warn | alarm
    text  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);

-- ---- Leveranskö ------------------------------------------------------
--
-- Larm som ska ut på en kanal. Misslyckade utskick görs om med växande
-- fördröjning tills de lyckas eller ger upp.
--
-- Kön överlever omstart. Det är hela poängen: ett larm får inte
-- försvinna för att tjänsten startades om mitt i ett utskick.

CREATE TABLE IF NOT EXISTS deliveries (
    id           INTEGER PRIMARY KEY,
    channel      TEXT NOT NULL,        -- webhook | smtp | mqtt | radio | sms
    device       TEXT NOT NULL,
    event        TEXT NOT NULL,        -- down | up
    payload      TEXT NOT NULL,        -- JSON
    status       TEXT NOT NULL,        -- pending | sent | failed
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL,
    last_error   TEXT,
    created_at   INTEGER NOT NULL,
    sent_at      INTEGER
);

CREATE INDEX IF NOT EXISTS idx_deliveries_pending
    ON deliveries(status, next_attempt);

-- ---- Underhållsfönster -----------------------------------------------
--
-- Flyttade hit från localStorage. I serverläge är de delade mellan
-- användare och måste överleva att en webbläsare rensas.
--
-- Två scheman:
--   once   engångsfönster mellan två tidpunkter (ms)
--   daily  återkommande i LOKAL tid, valfritt begränsat till veckodagar,
--          hanterar spann över midnatt
--
-- Tre måltyper: alla enheter, en grupp, eller en uttrycklig lista.
-- Listan ligger i maintenance_targets i stället för som JSON, så att
-- borttagna enheter städas bort automatiskt via ON DELETE CASCADE.

CREATE TABLE IF NOT EXISTS maintenance_windows (
    id              INTEGER PRIMARY KEY,
    label           TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,

    kind            TEXT NOT NULL,      -- once | daily

    -- kind = once
    starts_at       INTEGER,
    ends_at         INTEGER,

    -- kind = daily
    start_min       INTEGER,            -- minuter från midnatt, lokal tid
    duration_min    INTEGER,
    days            TEXT,               -- "0,1,2" (0 = söndag). Tomt = alla

    target_kind     TEXT NOT NULL,      -- all | group | hosts
    target_group_id INTEGER REFERENCES groups(id) ON DELETE CASCADE,

    created_at      INTEGER NOT NULL,
    created_by      TEXT                -- användarnamn, etapp 5
);

CREATE INDEX IF NOT EXISTS idx_maint_enabled ON maintenance_windows(enabled);

-- Enskilda enheter för target_kind = hosts.
CREATE TABLE IF NOT EXISTS maintenance_targets (
    window_id INTEGER NOT NULL REFERENCES maintenance_windows(id) ON DELETE CASCADE,
    host_id   INTEGER NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    PRIMARY KEY (window_id, host_id)
);
