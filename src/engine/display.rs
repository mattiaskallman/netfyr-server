// =====================================================================
// engine/display.rs
// Härledd visningsstatus.
//
// Ren modul. Bakgrunden: det råa ping-utfallet säger om enheten svarade
// nyss, medan larm styrs av den BEKRÄFTADE statusen från flap-grinden.
// Tidigare läste gränssnitt, räknare och hårdvara rådatat medan larmen
// gick på grinden — så en enstaka missad ping färgade allt rött utan att
// något larm gick.
//
// Regeln är enkel: RÖTT betyder "larmgrinden har släppt igenom". Allt
// däremellan får egna, dämpade lägen som aldrig når hårdvara eller
// larmkanaler.
//
// Portad från desktopvariantens hostDisplay.ts.
// =====================================================================

use super::types::{DisplayStatus, Host, ProbeType, RawStatus, Status, SuppressionReason};

/// Enda stället där visningsstatus bestäms.
///
/// Prioritet uppifrån och ned: pausad > undertryckt > bekräftat nere >
/// enstaka miss > långsam > uppe.
pub fn display_status_of(host: &Host, reason: Option<SuppressionReason>) -> DisplayStatus {
    if !host.enabled {
        return DisplayStatus::Paused;
    }
    if reason.is_some() {
        return DisplayStatus::Suppressed;
    }
    if host.confirmed == Status::Down {
        // Grinden säger fortfarande NER. Svarar enheten igen är
        // återställningen på väg, men larmet lever kvar tills
        // success-perioden löpt ut — därför ett eget läge.
        return if host.raw == RawStatus::Offline {
            DisplayStatus::Offline
        } else {
            DisplayStatus::Recovering
        };
    }
    if host.raw == RawStatus::Offline {
        return DisplayStatus::Uncertain;
    }
    match host.raw {
        RawStatus::Warning => DisplayStatus::Warning,
        _ => DisplayStatus::Online,
    }
}

/// Larmar den här enheten just nu? Det enda som får ge rött.
pub fn is_alarming(host: &Host, reason: Option<SuppressionReason>) -> bool {
    host.enabled && reason.is_none() && host.confirmed == Status::Down
}

/// Lägen som ska flyta upp i listan: allt som inte är lugnt, pausat
/// eller undertryckt.
pub fn is_problem_display(s: DisplayStatus) -> bool {
    matches!(
        s,
        DisplayStatus::Offline
            | DisplayStatus::Recovering
            | DisplayStatus::Warning
            | DisplayStatus::Uncertain
    )
}

/// Räknare över visningslägen — underlag för sammanfattning och
/// eventuell statuslampa.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DisplayTally {
    pub total: usize,
    /// Antal enheter som är påslagna.
    pub active: usize,
    pub online: usize,
    pub warning: usize,
    /// uncertain + recovering — dämpat läge, rör inte hårdvaran.
    pub uncertain: usize,
    /// Bekräftat nere med levererat larm, exklusive recovering.
    pub offline: usize,
    /// Alla undertryckta, oavsett underliggande läge.
    pub suppressed: usize,
    /// Undertryckta som ÄR bekräftat nere.
    pub suppressed_down: usize,
    pub paused: usize,
    /// Bekräftat nere och inte undertryckt. Enda källan till rött.
    pub alarming: usize,
}

/// Summera visningslägen för hela enhetslistan.
///
/// `reason_of` skickas in i stället för att räknas ut här, eftersom
/// undertryckning behöver underhållsfönster och en uppslagning som
/// anroparen redan har.
pub fn tally_hosts(
    hosts: &[Host],
    reason_of: &dyn Fn(&Host) -> Option<SuppressionReason>,
) -> DisplayTally {
    let mut out = DisplayTally {
        total: hosts.len(),
        ..Default::default()
    };

    for h in hosts {
        if !h.enabled {
            out.paused += 1;
            continue;
        }
        out.active += 1;

        let reason = reason_of(h);
        let s = display_status_of(h, reason);
        if is_alarming(h, reason) {
            out.alarming += 1;
        }

        match s {
            DisplayStatus::Online => out.online += 1,
            DisplayStatus::Warning => out.warning += 1,
            DisplayStatus::Uncertain | DisplayStatus::Recovering => out.uncertain += 1,
            DisplayStatus::Offline => out.offline += 1,
            DisplayStatus::Suppressed => {
                out.suppressed += 1;
                if h.confirmed == Status::Down {
                    out.suppressed_down += 1;
                }
            }
            DisplayStatus::Paused => {}
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(confirmed: Status, raw: RawStatus, enabled: bool) -> Host {
        Host {
            id: 1,
            name: "test".to_string(),
            address: "10.0.0.1".to_string(),
            group_id: None,
            group: None,
            note: None,
            enabled,
            sms_enabled: true,
            confirmed,
            raw,
            snooze_until: None,
            depends_on_address: None,
            interval_sec: None,
            packet_size: None,
            slow_threshold_ms: None,
            fail_period_sec: None,
            success_period_sec: None,
            probe_type: ProbeType::Icmp,
            probe_port: None,
        }
    }

    #[test]
    fn enstaka_miss_ger_uncertain_inte_offline() {
        // Rådatat säger offline men grinden har inte bekräftat.
        let h = host(Status::Up, RawStatus::Offline, true);
        assert_eq!(display_status_of(&h, None), DisplayStatus::Uncertain);
        assert!(!is_alarming(&h, None));
    }

    #[test]
    fn bekraftat_nere_ger_offline() {
        let h = host(Status::Down, RawStatus::Offline, true);
        assert_eq!(display_status_of(&h, None), DisplayStatus::Offline);
        assert!(is_alarming(&h, None));
    }

    #[test]
    fn svar_under_bekraftat_nere_ger_recovering() {
        // Grinden säger fortfarande nere, men enheten svarar igen.
        let h = host(Status::Down, RawStatus::Online, true);
        assert_eq!(display_status_of(&h, None), DisplayStatus::Recovering);
        // Larmet lever kvar tills success-perioden löpt ut.
        assert!(is_alarming(&h, None));
    }

    #[test]
    fn pausad_vinner_over_allt() {
        let h = host(Status::Down, RawStatus::Offline, false);
        assert_eq!(display_status_of(&h, None), DisplayStatus::Paused);
        assert!(!is_alarming(&h, None));
    }

    #[test]
    fn undertryckt_larmar_aldrig() {
        let h = host(Status::Down, RawStatus::Offline, true);
        let r = Some(SuppressionReason::Maintenance);
        assert_eq!(display_status_of(&h, r), DisplayStatus::Suppressed);
        assert!(!is_alarming(&h, r));
    }

    #[test]
    fn langsam_ger_warning() {
        let h = host(Status::Up, RawStatus::Warning, true);
        assert_eq!(display_status_of(&h, None), DisplayStatus::Warning);
    }

    #[test]
    fn tally_raknar_ratt() {
        let hosts = vec![
            host(Status::Up, RawStatus::Online, true),
            host(Status::Up, RawStatus::Warning, true),
            host(Status::Up, RawStatus::Offline, true),   // uncertain
            host(Status::Down, RawStatus::Offline, true), // offline, alarming
            host(Status::Down, RawStatus::Online, true),  // recovering, alarming
            host(Status::Up, RawStatus::Online, false),   // paused
        ];
        let t = tally_hosts(&hosts, &|_| None);

        assert_eq!(t.total, 6);
        assert_eq!(t.active, 5);
        assert_eq!(t.online, 1);
        assert_eq!(t.warning, 1);
        assert_eq!(t.uncertain, 2); // uncertain + recovering
        assert_eq!(t.offline, 1);
        assert_eq!(t.paused, 1);
        assert_eq!(t.alarming, 2);
    }

    #[test]
    fn undertryckt_nere_raknas_separat() {
        let hosts = vec![host(Status::Down, RawStatus::Offline, true)];
        let t = tally_hosts(&hosts, &|_| Some(SuppressionReason::Snooze));
        assert_eq!(t.suppressed, 1);
        assert_eq!(t.suppressed_down, 1);
        assert_eq!(t.alarming, 0);
    }
}
