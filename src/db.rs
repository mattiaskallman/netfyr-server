// =====================================================================
// db.rs
// Databaslager.
//
// rusqlite är blockerande. All DB-åtkomst går därför genom
// spawn_blocking så att tokios arbetartrådar aldrig blockeras.
//
// En enda anslutning bakom ett Mutex räcker gott för NetFyr: skrivarna
// är få (pollingloopen och enstaka operatörsåtgärder) och WAL-läget gör
// att läsare inte blockerar skrivare. Blir samtidigheten ett problem
// senare är en pool rätt svar — men att införa den nu vore att lösa ett
// problem vi inte har.
// =====================================================================

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Aktuell schemaversion. Höjs vid varje migrationssteg.
const SCHEMA_VERSION: i32 = 7;

const SCHEMA_SQL: &str = include_str!("schema.sql");
const MIGRATION_V2_SQL: &str = include_str!("migrations/v2_auth.sql");
const MIGRATION_V3_SQL: &str = include_str!("migrations/v3_sms_sessions.sql");
const MIGRATION_V4_SQL: &str = include_str!("migrations/v4_host_raw.sql");
const MIGRATION_V5_SQL: &str = include_str!("migrations/v5_probes.sql");
const MIGRATION_V6_SQL: &str = include_str!("migrations/v6_host_sms.sql");
const MIGRATION_V7_SQL: &str = include_str!("migrations/v7_session_activity.sql");

fn migration_transaction<F>(conn: &mut Connection, target_version: i32, apply: F) -> Result<()>
where
    F: FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
{
    let tx = conn
        .transaction()
        .context("kunde inte starta schematransaktion")?;
    apply(&tx)?;
    tx.pragma_update(None, "user_version", target_version)?;
    tx.commit()
        .context("kunde inte slutföra schematransaktion")?;
    Ok(())
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Öppna databasen och kör migrationer.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("kunde inte skapa {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("kunde inte öppna databasen {}", path.display()))?;

        // WAL: läsare blockerar inte skrivare. Avgörande när
        // pollingloopen skriver samtidigt som gränssnittet läser.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Vänta hellre än att returnera SQLITE_BUSY direkt.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Kör schemat och stega upp user_version.
    ///
    /// Schemat är idempotent (allt är IF NOT EXISTS), så det kan köras om
    /// utan skada. Framtida ändringar läggs som nya steg i match-satsen
    /// nedan i stället för att redigera schema.sql — annars kan befintliga
    /// installationer inte uppgraderas.
    fn migrate(&self) -> Result<()> {
        let mut conn = self.lock();
        let current: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

        if current > SCHEMA_VERSION {
            anyhow::bail!(
                "databasen har schemaversion {current}, men den här binären förstår \
                 högst {SCHEMA_VERSION}. Uppgradera NetFyr Server."
            );
        }

        if current == SCHEMA_VERSION {
            tracing::debug!("schemaversion {current}, inget att göra");
            return Ok(());
        }

        tracing::info!("migrerar schema {current} → {SCHEMA_VERSION}");
        migration_transaction(&mut conn, SCHEMA_VERSION, |tx| {
            // Steg 0 → 1: grundschemat.
            if current < 1 {
                tx.execute_batch(SCHEMA_SQL)
                    .context("kunde inte köra grundschemat")?;
            }

            // Steg 1 → 2: användare, sessioner och auditlogg (etapp 5).
            if current < 2 {
                tx.execute_batch(MIGRATION_V2_SQL)
                    .context("kunde inte migrera till schemaversion 2")?;
            }

            // Steg 2 → 3: SMS-eskaleringssessioner (etapp 7).
            if current < 3 {
                tx.execute_batch(MIGRATION_V3_SQL)
                    .context("kunde inte migrera till schemaversion 3")?;
            }

            // Steg 3 → 4: råstatus i host_status.
            if current < 4 {
                tx.execute_batch(MIGRATION_V4_SQL)
                    .context("kunde inte migrera till schemaversion 4")?;
            }

            // Steg 4 → 5: probetyp och port per enhet (etapp 8).
            if current < 5 {
                tx.execute_batch(MIGRATION_V5_SQL)
                    .context("kunde inte migrera till schemaversion 5")?;
            }

            // Steg 5 → 6: SMS-larm per enhet.
            if current < 6 {
                tx.execute_batch(MIGRATION_V6_SQL)
                    .context("kunde inte migrera till schemaversion 6")?;
            }

            // Steg 6 → 7: mänsklig sessionsaktivitet.
            if current < 7 {
                tx.execute_batch(MIGRATION_V7_SQL)
                    .context("kunde inte migrera till schemaversion 7")?;
            }
            Ok(())
        })
    }

    /// Ett förgiftat lås får aldrig fälla tjänsten — ta över innehållet.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Kör en blockerande operation utanför tokios arbetartrådar.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&guard)
        })
        .await
        .context("databastråden kraschade")?
    }

    /// Enkel hälsokontroll. Verifierar att anslutningen svarar.
    pub async fn ping(&self) -> Result<()> {
        self.call(|conn| {
            conn.query_row("SELECT 1", [], |r| r.get::<_, i32>(0))?;
            Ok(())
        })
        .await
    }

    /// Antal enheter. Används av hälsokontrollen för att visa att
    /// schemat faktiskt är på plats och läsbart.
    pub async fn host_count(&self) -> Result<i64> {
        self.call(|conn| {
            let n = conn.query_row("SELECT COUNT(*) FROM hosts", [], |r| r.get(0))?;
            Ok(n)
        })
        .await
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn misslyckad_migration_rullar_tillbaka_schema_och_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE sample (id INTEGER)", [])
            .unwrap();

        let result = migration_transaction(&mut conn, 7, |tx| {
            tx.execute("ALTER TABLE sample ADD COLUMN added INTEGER", [])?;
            anyhow::bail!("injicerat avbrott");
        });

        assert!(result.is_err());
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        let added: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('sample') WHERE name='added'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, 0);
        assert_eq!(added, 0);
    }

    #[test]
    fn v7_initierar_senaste_aktivitet_för_befintliga_sessioner() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                token_hash TEXT PRIMARY KEY,
                user_id INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                ip TEXT,
                user_agent TEXT
             );
             INSERT INTO sessions VALUES ('abc', 1, 1000, 9999999999999, NULL, NULL);",
        )
        .unwrap();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        conn.execute_batch(MIGRATION_V7_SQL).unwrap();

        let last_activity: i64 = conn
            .query_row(
                "SELECT last_activity FROM sessions WHERE token_hash = 'abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(last_activity >= before - 1000 && last_activity <= after);
    }
}
