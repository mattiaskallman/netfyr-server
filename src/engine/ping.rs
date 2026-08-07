// =====================================================================
// engine/ping.rs
// ICMP-ping.
//
// Använder ICMP-DATAGRAMSOCKETAR, inte råa socketar. Skillnaden är
// avgörande i container: datagramvarianten kräver ingen CAP_NET_RAW så
// länge processens grupp ryms inom net.ipv4.ping_group_range.
//
// Det är samma väg som iputils-ping tar, och den vi verifierade innan
// motorn skrevs. Kör tjänsten som en användare vars GID ligger utanför
// intervallet misslyckas varje ping med "Operation not permitted" —
// felet loggas då en gång per svep och inte per enhet.
// =====================================================================

use anyhow::{Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use surge_ping::{Client, Config, PingIdentifier, PingSequence};

/// Löpnummer för ICMP-sekvens. Wrappar av sig själv.
static SEQ: AtomicU16 = AtomicU16::new(0);

pub struct Pinger {
    client: Client,
    identifier: u16,
}

impl Pinger {
    pub fn new() -> Result<Self> {
        let client = Client::new(&Config::default())
            .context("kunde inte skapa ICMP-socket (kontrollera ping_group_range)")?;

        // Identifieraren skiljer våra svar från andra processers. Låg
        // upplösning räcker — vi matchar även på adress och sekvens.
        let identifier = std::process::id() as u16;

        Ok(Self { client, identifier })
    }

    /// Slå upp en adress till IP.
    ///
    /// Adressen kan vara både IP och värdnamn. I en air-gappad
    /// installation utan DNS bör värdnamn undvikas, men det avgör
    /// användaren.
    pub async fn resolve(address: &str) -> Result<IpAddr> {
        if let Ok(ip) = address.parse::<IpAddr>() {
            return Ok(ip);
        }
        let mut addrs = tokio::net::lookup_host((address, 0))
            .await
            .with_context(|| format!("kunde inte slå upp {address}"))?;
        addrs
            .next()
            .map(|s: SocketAddr| s.ip())
            .with_context(|| format!("ingen adress för {address}"))
    }

    /// Pinga en adress. Returnerar svarstid, eller None vid uteblivet svar.
    ///
    /// Ett uteblivet svar är inte ett fel i programmets mening — det är
    /// själva mätvärdet. Err reserveras för att socketen inte gick att
    /// använda alls.
    pub async fn ping(
        &self,
        address: &str,
        payload_size: usize,
        timeout: Duration,
    ) -> Result<Option<Duration>> {
        let ip = Self::resolve(address).await?;

        let mut pinger = self.client.pinger(ip, PingIdentifier(self.identifier)).await;
        pinger.timeout(timeout);

        let payload = vec![0u8; payload_size.clamp(16, 1400)];
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);

        match pinger.ping(PingSequence(seq), &payload).await {
            Ok((_packet, rtt)) => Ok(Some(rtt)),
            Err(surge_ping::SurgeError::Timeout { .. }) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("ping mot {address} misslyckades: {e}")),
        }
    }
}
