// =====================================================================
// engine/flap.rs
// Tidsbaserad hysteres ("flapp-dämpning").
//
// Ren modul: ingen I/O, ingen databas, inga nätverksanrop. Tar emot
// poll-utfall och avgör när en BEKRÄFTAD statusövergång sker.
// Persistering och larmköläggning sker i pollingloopen som anropar
// register_poll — det här är bara beslutslogiken.
//
// Portad rad för rad från desktopvariantens flapDamping.ts, som i sin
// tur portades från en tidigare monitor.rs. Samma tillståndsmaskin,
// samma tester. Avvikelser här är buggar, inte förbättringar.
// =====================================================================

use super::types::{Outcome, Status};

/// Globala standardvärden.
pub const FLAP_FAIL_SEC_DEFAULT: u32 = 30;
pub const FLAP_SUCCESS_SEC_DEFAULT: u32 = 30;

/// Tröskelvärden, upplösta per enhet (globalt + valfri override).
#[derive(Debug, Clone, Copy)]
pub struct HysteresisConfig {
    /// Sammanhängande miss-tid innan enheten bekräftas som NER.
    pub fail_period_sec: u32,
    /// Sammanhängande svars-tid innan enheten bekräftas som UPP.
    pub success_period_sec: u32,
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self {
            fail_period_sec: FLAP_FAIL_SEC_DEFAULT,
            success_period_sec: FLAP_SUCCESS_SEC_DEFAULT,
        }
    }
}

/// En bekräftad statusövergång. Anroparen avgör om den ska larma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusChange {
    pub from: Status,
    pub to: Status,
}

/// Bekräftad status plus pågående svit.
///
/// Endast `status` persisteras. Sviten börjar färsk vid omstart — en
/// svit som påbörjades före en omstart säger inget om läget efteråt.
#[derive(Debug, Clone, Copy)]
pub struct HostMonitorState {
    pub status: Status,
    pub streak_outcome: Outcome,
    pub streak_started_at_ms: i64,
}

impl HostMonitorState {
    /// Enhet utan persisterad status.
    pub fn new(now_ms: i64) -> Self {
        Self {
            status: Status::Unknown,
            streak_outcome: Outcome::Success,
            streak_started_at_ms: now_ms,
        }
    }

    /// Återskapad från persisterad, bekräftad status vid omstart.
    pub fn from_persisted(status: Status, now_ms: i64) -> Self {
        let streak_outcome = if status == Status::Down {
            Outcome::Fail
        } else {
            Outcome::Success
        };
        Self {
            status,
            streak_outcome,
            streak_started_at_ms: now_ms,
        }
    }
}

/// Mata in ett poll-utfall vid tidpunkten `now_ms`.
///
/// Muterar tillståndet och returnerar Some ENBART när en bekräftad
/// övergång sker, alltså där larm eller återställning ska köas. Annars
/// None: enstaka flapp inom perioden absorberas tyst.
pub fn register_poll(
    state: &mut HostMonitorState,
    outcome: Outcome,
    now_ms: i64,
    cfg: &HysteresisConfig,
) -> Option<StatusChange> {
    // Utfallet vänder mot pågående svit: nollställ sviten, rör inte status.
    if outcome != state.streak_outcome {
        state.streak_outcome = outcome;
        state.streak_started_at_ms = now_ms;
        return None;
    }

    // Samma utfall — hur länge har sviten pågått?
    let streak_len_ms = (now_ms - state.streak_started_at_ms).max(0);

    // Upp eller okänd + tillräckligt lång miss-svit -> bekräfta NER.
    if matches!(state.status, Status::Up | Status::Unknown)
        && outcome == Outcome::Fail
        && streak_len_ms >= i64::from(cfg.fail_period_sec) * 1000
    {
        let from = state.status;
        state.status = Status::Down;
        return Some(StatusChange {
            from,
            to: Status::Down,
        });
    }

    // Ner eller okänd + tillräckligt lång svars-svit -> bekräfta UPP.
    if matches!(state.status, Status::Down | Status::Unknown)
        && outcome == Outcome::Success
        && streak_len_ms >= i64::from(cfg.success_period_sec) * 1000
    {
        let from = state.status;
        state.status = Status::Up;
        return Some(StatusChange {
            from,
            to: Status::Up,
        });
    }

    // Redan i målstatus, eller perioden ännu inte uppnådd.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(fail: u32, success: u32) -> HysteresisConfig {
        HysteresisConfig {
            fail_period_sec: fail,
            success_period_sec: success,
        }
    }

    #[test]
    fn enstaka_miss_ger_ingen_overgang() {
        let c = cfg(30, 30);
        let mut s = HostMonitorState::from_persisted(Status::Up, 0);

        // En miss vänder sviten men bekräftar inget.
        assert_eq!(register_poll(&mut s, Outcome::Fail, 1_000, &c), None);
        // Svar igen: sviten vänder tillbaka, status orörd.
        assert_eq!(register_poll(&mut s, Outcome::Success, 2_000, &c), None);
        assert_eq!(s.status, Status::Up);
    }

    #[test]
    fn sammanhangande_miss_bekraftar_ner() {
        let c = cfg(30, 30);
        let mut s = HostMonitorState::from_persisted(Status::Up, 0);

        assert_eq!(register_poll(&mut s, Outcome::Fail, 1_000, &c), None);
        // 29 sekunder in i sviten: ännu inte.
        assert_eq!(register_poll(&mut s, Outcome::Fail, 30_000, &c), None);
        // 30 sekunder: bekräftad.
        let change = register_poll(&mut s, Outcome::Fail, 31_000, &c);
        assert_eq!(
            change,
            Some(StatusChange {
                from: Status::Up,
                to: Status::Down
            })
        );
        assert_eq!(s.status, Status::Down);
    }

    #[test]
    fn bekraftar_bara_en_gang() {
        let c = cfg(30, 30);
        let mut s = HostMonitorState::from_persisted(Status::Up, 0);
        register_poll(&mut s, Outcome::Fail, 1_000, &c);
        register_poll(&mut s, Outcome::Fail, 31_000, &c);
        assert_eq!(s.status, Status::Down);
        // Fortsatta missar ger inga fler övergångar.
        assert_eq!(register_poll(&mut s, Outcome::Fail, 99_000, &c), None);
    }

    #[test]
    fn aterstallning_kraver_egen_period() {
        let c = cfg(30, 60);
        let mut s = HostMonitorState::from_persisted(Status::Down, 0);

        assert_eq!(register_poll(&mut s, Outcome::Success, 1_000, &c), None);
        // 59 sekunder: fortfarande nere, larmet lever.
        assert_eq!(register_poll(&mut s, Outcome::Success, 60_000, &c), None);
        assert_eq!(s.status, Status::Down);
        // 60 sekunder: uppe.
        let change = register_poll(&mut s, Outcome::Success, 61_000, &c);
        assert_eq!(
            change,
            Some(StatusChange {
                from: Status::Down,
                to: Status::Up
            })
        );
    }

    #[test]
    fn okand_kan_ga_at_bada_hallen() {
        let c = cfg(30, 30);

        let mut up = HostMonitorState::new(0);
        assert_eq!(
            register_poll(&mut up, Outcome::Success, 31_000, &c),
            Some(StatusChange {
                from: Status::Unknown,
                to: Status::Up
            })
        );

        let mut down = HostMonitorState::new(0);
        // Första misslyckandet vänder sviten från default Success.
        assert_eq!(register_poll(&mut down, Outcome::Fail, 0, &c), None);
        assert_eq!(
            register_poll(&mut down, Outcome::Fail, 31_000, &c),
            Some(StatusChange {
                from: Status::Unknown,
                to: Status::Down
            })
        );
    }

    #[test]
    fn bakatgaende_klocka_ger_ingen_panik() {
        let c = cfg(30, 30);
        let mut s = HostMonitorState::from_persisted(Status::Up, 100_000);
        // Klockan hoppar bakåt: svitlängden klampas till 0, inget bekräftas.
        assert_eq!(register_poll(&mut s, Outcome::Fail, 0, &c), None);
        assert_eq!(s.status, Status::Up);
    }
}
