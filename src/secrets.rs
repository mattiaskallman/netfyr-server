// =====================================================================
// secrets.rs
// Hemligheter i krypterad fil.
//
// Desktopvarianten använder operativsystemets nyckelhanterare. Det
// fungerar inte headless: keyring-crateten går över Secret Service på
// D-Bus, och i en container utan skrivbordssession finns varken
// D-Bus-session eller gnome-keyring.
//
// Här ligger hemligheterna i stället i en fil krypterad med
// XChaCha20-Poly1305. Nyckeln ligger i en egen fil med 0600, ägd av
// tjänstanvändaren.
//
// SÄKERHETSMODELLEN ÄR ÄRLIG OM SINA GRÄNSER: nyckeln ligger på samma
// maskin som datan. Skyddet gäller mot att hemligheter läcker med en
// databaskopia, en säkerhetskopia eller en felrapport — inte mot någon
// som redan har filsystemsåtkomst som tjänstanvändaren.
//
// Att kopiera secrets.enc utan secrets.key är meningslöst. Båda måste
// säkerhetskopieras, och nyckeln bör förvaras skilt från datan.
// =====================================================================

use anyhow::{bail, Context, Result};
// RngCore hämtas från aeads egen rand_core, inte från rand-crateten.
// chacha20poly1305 0.10 bygger på rand_core 0.6 medan rand 0.9 använder
// 0.10 — OsRng från den ena implementerar inte den andras trait.
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

#[derive(Clone)]
pub struct Secrets {
    path: PathBuf,
    cipher: Arc<XChaCha20Poly1305>,
    cache: Arc<Mutex<HashMap<String, String>>>,
}

impl Secrets {
    /// Öppna eller skapa hemlighetslagret.
    ///
    /// `dir` är normalt /var/lib/netfyr. Nyckeln skapas vid första start
    /// och får rättigheterna 0600.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("kunde inte skapa {}", dir.display()))?;

        let key_path = dir.join("secrets.key");
        let key = load_or_create_key(&key_path)?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key)
            .map_err(|e| anyhow::anyhow!("ogiltig nyckel: {e}"))?;

        let path = dir.join("secrets.enc");
        let cache = if path.exists() {
            decrypt_file(&cipher, &path)?
        } else {
            HashMap::new()
        };

        Ok(Self {
            path,
            cipher: Arc::new(cipher),
            cache: Arc::new(Mutex::new(cache)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hämta en hemlighet. None när den inte finns.
    pub fn get(&self, name: &str) -> Option<String> {
        self.lock().get(name).cloned()
    }

    /// Hämta en hemlighet, eller ett läsbart fel.
    ///
    /// Motsvarar secret_for() i desktopvarianten och används av kanaler
    /// som kräver ett lösenord.
    pub fn require(&self, name: &str) -> Result<String> {
        self.get(name)
            .with_context(|| format!("kanalen {name} saknar hemlighet"))
    }

    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        {
            let mut c = self.lock();
            c.insert(name.to_string(), value.to_string());
        }
        self.flush()
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        {
            let mut c = self.lock();
            c.remove(name);
        }
        self.flush()
    }

    /// Namn på lagrade hemligheter. Aldrig värdena — den här listan går
    /// till gränssnittet för att visa vilka kanaler som är konfigurerade.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lock().keys().cloned().collect();
        v.sort();
        v
    }

    fn flush(&self) -> Result<()> {
        let json = serde_json::to_vec(&*self.lock())?;

        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, json.as_ref())
            .map_err(|e| anyhow::anyhow!("kryptering misslyckades: {e}"))?;

        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);

        // Skriv till temporärfil och byt in den. Ett strömavbrott mitt i
        // en skrivning får inte lämna en halv fil — då vore samtliga
        // hemligheter förlorade.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &out)?;
        restrict(&tmp)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn load_or_create_key(path: &Path) -> Result<[u8; KEY_LEN]> {
    if path.exists() {
        let bytes = std::fs::read(path)
            .with_context(|| format!("kunde inte läsa {}", path.display()))?;
        if bytes.len() != KEY_LEN {
            bail!(
                "{} har fel längd ({} byte, förväntat {KEY_LEN})",
                path.display(),
                bytes.len()
            );
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }

    let mut key = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut key);
    std::fs::write(path, key)?;
    restrict(path)?;
    tracing::info!("skapade ny hemlighetsnyckel: {}", path.display());
    tracing::warn!("säkerhetskopiera {} — utan den går hemligheterna inte att läsa", path.display());
    Ok(key)
}

/// Sätt 0600. Utan detta kan vilken lokal användare som helst läsa
/// nyckeln, och hela krypteringen blir en formalitet.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

fn decrypt_file(cipher: &XChaCha20Poly1305, path: &Path) -> Result<HashMap<String, String>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("kunde inte läsa {}", path.display()))?;

    if bytes.len() <= NONCE_LEN {
        bail!("{} är trunkerad", path.display());
    }

    let (nonce_bytes, ciphertext) = bytes.split_at(NONCE_LEN);
    let plaintext = cipher
        .decrypt(XNonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|_| {
            anyhow::anyhow!(
                "kunde inte dekryptera {} — fel nyckel, eller filen är skadad",
                path.display()
            )
        })?;

    Ok(serde_json::from_slice(&plaintext)?)
}
