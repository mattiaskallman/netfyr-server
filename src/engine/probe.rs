// =====================================================================
// engine/probe.rs
// Tjänsteprober: TCP-anslutning och HTTP-fråga (etapp 8).
//
// Ping säger bara att OPERATIVSYSTEMET lever. En tjänst som hängt —
// webbservern svarar inte, databasen lyssnar inte — syns inte på ping.
// Proberna här svarar på frågan operatören faktiskt ställer: "svarar
// TJÄNSTEN?"
//
// Samma felkontrakt som ping.rs: Ok(None) är ett MÄTVÄRDE (inget svar),
// inte ett fel. Err reserveras för när vi inte kunde mäta alls, och
// loggas en gång per svep av monitorn — inte per enhet.
//
// HTTP är medvetet enkelt: GET mot /, 2xx/3xx räknas som svar. En 500
// betyder att tjänsten är sjuk och räknas som miss — samma princip som
// Uptime Kumas standard. HTTPS stöds inte: nätet är air-gappat och
// tjänsterna där kör klartext eller egen CA. (Är TLS en dag aktuellt
// hör det hemma i certövervakning, inte i den här proben.)
// =====================================================================

use anyhow::Result;
use std::time::{Duration, Instant};

/// TCP: kan vi öppna en anslutning till adress:port?
///
/// Svarstiden är tiden till upprättad anslutning — samma mätvärde som
/// pingens RTT, så statistik och trösklar fungerar oförändrat.
pub async fn tcp_check(address: &str, port: u16, timeout: Duration) -> Result<Option<Duration>> {
    let start = Instant::now();
    let target = (address, port);

    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(target)).await {
        Ok(Ok(_stream)) => Ok(Some(start.elapsed())),
        // Timeout och nekad anslutning är samma mätvärde: inget svar.
        Ok(Err(_)) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// HTTP: svarar tjänsten på en GET mot / med 2xx/3xx?
///
/// Svarstiden mäts till svarshuvudena — vi läser aldrig bodyn. Att hämta
/// hela svaret skulle göra mätningen beroende av sidans storlek, inte av
/// tjänstens hälsa.
pub async fn http_check(
    client: &reqwest::Client,
    address: &str,
    port: u16,
    timeout: Duration,
) -> Result<Option<Duration>> {
    // Adressen är en IP eller ett värdnamn ur hosts — aldrig en URL, så
    // den kan inte smuggla in schema, sökväg eller credentials.
    let url = format!("http://{address}:{port}/");
    let start = Instant::now();

    let req = client.get(&url).timeout(timeout);
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() || status.is_redirection() {
                Ok(Some(start.elapsed()))
            } else {
                Ok(None)
            }
        }
        Err(e) if e.is_timeout() || e.is_connect() => Ok(None),
        // Avkodnings- och protokollfel är också "inget giltigt svar" —
        // tjänsten talade inte HTTP. Err är reserverat för när vi inte
        // kunde försöka alls (t.ex. ogiltig adresssträng).
        Err(e) if e.is_request() || e.is_decode() => Ok(None),
        Err(e) => Err(anyhow::anyhow!("http-mätning mot {url} misslyckades: {e}")),
    }
}
