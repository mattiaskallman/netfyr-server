// =====================================================================
// engine/suppression.rs
// Undertryckningslogik.
//
// Ren modul. Avgör om — och varför — en enhets larm ska tystas. Används
// på två ställen: larmgrinden (håller tillbaka leverans) och
// gränssnittet (dämpar, sorterar, märker).
//
// Att allt går genom en punkt håller snooze, underhållsfönster och
// beroenden konsekventa: var och en är bara ytterligare en anledning
// som samma grind returnerar.
//
// Portad från desktopvariantens suppression.ts.
// =====================================================================

use chrono::{Local, TimeZone, Timelike};

use super::types::{Host, Status, SuppressionReason};

const DAY_MS: i64 = 86_400_000;

/// Schemaläggning för ett underhållsfönster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Engångsfönster mellan två tidpunkter (ms).
    Once { start: i64, end: i64 },
    /// Återkommande fönster i lokal tid, valfritt begränsat till vissa
    /// veckodagar. Hanterar spann över midnatt.
    Daily {
        start_min: u32,
        duration_min: u32,
        /// 0 = söndag … 6 = lördag. Tom lista = alla dagar.
        days: Vec<u8>,
    },
}

/// Vilka enheter ett fönster gäller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    All,
    Group(i64),
    Hosts(Vec<i64>),
}

#[derive(Debug, Clone)]
pub struct MaintenanceWindow {
    pub id: i64,
    pub enabled: bool,
    pub schedule: Schedule,
    pub target: Target,
}

/// Lokala minuter från midnatt för en tidsstämpel.
fn local_minutes(now_ms: i64) -> u32 {
    match Local.timestamp_millis_opt(now_ms).single() {
        Some(dt) => dt.hour() * 60 + dt.minute(),
        None => 0,
    }
}

/// Lokal veckodag, 0 = söndag … 6 = lördag.
fn local_day(now_ms: i64) -> u8 {
    match Local.timestamp_millis_opt(now_ms).single() {
        Some(dt) => {
            use chrono::Datelike;
            dt.weekday().num_days_from_sunday() as u8
        }
        None => 0,
    }
}

/// Är fönstret aktivt just nu? Tittar bara på schemat, inte på målet.
pub fn is_window_active(w: &MaintenanceWindow, now_ms: i64) -> bool {
    if !w.enabled {
        return false;
    }

    match &w.schedule {
        Schedule::Once { start, end } => now_ms >= *start && now_ms <= *end,
        Schedule::Daily {
            start_min,
            duration_min,
            days,
        } => {
            let dur = *duration_min;
            if dur == 0 {
                return false;
            }

            let now_min = local_minutes(now_ms);
            let today = local_day(now_ms);
            let day_ok = |d: u8| days.is_empty() || days.contains(&d);

            let end = start_min + dur;

            // Delen som ligger på samma dygn.
            if now_min >= *start_min && now_min < end.min(1440) && day_ok(today) {
                return true;
            }

            // Delen efter midnatt: fönstret startade föregående lokala dygn.
            if end > 1440 {
                let yesterday = local_day(now_ms - DAY_MS);
                if now_min < end - 1440 && day_ok(yesterday) {
                    return true;
                }
            }

            false
        }
    }
}

/// Omfattar fönstrets mål den här enheten?
pub fn window_targets_host(w: &MaintenanceWindow, host: &Host) -> bool {
    match &w.target {
        Target::All => true,
        Target::Group(g) => host.group_id == Some(*g),
        Target::Hosts(ids) => ids.contains(&host.id),
    }
}

/// Ligger enheten i något aktivt underhållsfönster just nu?
pub fn maintenance_active(host: &Host, windows: &[MaintenanceWindow], now_ms: i64) -> bool {
    windows
        .iter()
        .any(|w| window_targets_host(w, host) && is_window_active(w, now_ms))
}

/// Varför enhetens larm är undertryckta just nu, om de är det.
///
/// Prioritet: snooze (manuell, omedelbar) > underhåll (planerat) >
/// beroende (rotorsak).
///
/// Beroenderegeln undertrycker när den NÄRMASTE föräldern är bekräftat
/// nere av flap-grinden, inte när den bara missat en enstaka ping. Att
/// läsa rådatat här skulle låta ett tappat paket uppströms tysta alla
/// barn.
///
/// Transitiviteten uppstår av sig själv: varje nod kollar sin egen
/// förälder, så bara den översta nedlagda noden i en kedja larmar — och
/// en nod bakom en frisk förälder larmar fortfarande även om något
/// längre uppströms är nere.
pub fn suppression_reason(
    host: &Host,
    now_ms: i64,
    windows: &[MaintenanceWindow],
    by_address: &dyn Fn(&str) -> Option<Host>,
) -> Option<SuppressionReason> {
    if let Some(until) = host.snooze_until {
        if until > now_ms {
            return Some(SuppressionReason::Snooze);
        }
    }

    if maintenance_active(host, windows, now_ms) {
        return Some(SuppressionReason::Maintenance);
    }

    if let Some(parent_addr) = &host.depends_on_address {
        if let Some(parent) = by_address(parent_addr) {
            if parent.enabled && parent.confirmed == Status::Down {
                return Some(SuppressionReason::Dependency);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::RawStatus;

    fn host(id: i64, address: &str) -> Host {
        Host {
            id,
            name: format!("host{id}"),
            address: address.to_string(),
            group_id: None,
            group: None,
            note: None,
            enabled: true,
            sms_enabled: true,
            confirmed: Status::Up,
            raw: RawStatus::Online,
            snooze_until: None,
            depends_on_address: None,
            interval_sec: None,
            packet_size: None,
            slow_threshold_ms: None,
            fail_period_sec: None,
            success_period_sec: None,
            probe_type: crate::engine::types::ProbeType::Icmp,
            probe_port: None,
        }
    }

    fn once_window(start: i64, end: i64) -> MaintenanceWindow {
        MaintenanceWindow {
            id: 1,
            enabled: true,
            schedule: Schedule::Once { start, end },
            target: Target::All,
        }
    }

    #[test]
    fn engangsfonster_galler_inom_spannet() {
        let h = host(1, "10.0.0.1");
        let w = vec![once_window(1_000, 2_000)];
        assert!(!is_window_active(&w[0], 999));
        assert!(is_window_active(&w[0], 1_500));
        assert!(!is_window_active(&w[0], 2_001));
        assert_eq!(
            suppression_reason(&h, 1_500, &w, &|_| None),
            Some(SuppressionReason::Maintenance)
        );
    }

    #[test]
    fn avslaget_fonster_galler_aldrig() {
        let mut w = once_window(0, i64::MAX);
        w.enabled = false;
        assert!(!is_window_active(&w, 1_000));
    }

    #[test]
    fn nolltid_ger_inget_fonster() {
        let w = MaintenanceWindow {
            id: 1,
            enabled: true,
            schedule: Schedule::Daily {
                start_min: 60,
                duration_min: 0,
                days: vec![],
            },
            target: Target::All,
        };
        assert!(!is_window_active(&w, 0));
    }

    #[test]
    fn snooze_vinner_over_underhall() {
        let mut h = host(1, "10.0.0.1");
        h.snooze_until = Some(5_000);
        let w = vec![once_window(0, 10_000)];
        assert_eq!(
            suppression_reason(&h, 1_000, &w, &|_| None),
            Some(SuppressionReason::Snooze)
        );
        // Efter att snoozen löpt ut tar underhållet över.
        assert_eq!(
            suppression_reason(&h, 6_000, &w, &|_| None),
            Some(SuppressionReason::Maintenance)
        );
    }

    #[test]
    fn beroende_kraver_bekraftad_forelder() {
        let mut child = host(2, "10.0.0.2");
        child.depends_on_address = Some("10.0.0.1".to_string());

        // Förälder uppe: barnet larmar.
        let parent_up = host(1, "10.0.0.1");
        assert_eq!(
            suppression_reason(&child, 0, &[], &|_| Some(parent_up.clone())),
            None
        );

        // Förälder bekräftat nere: barnet tystas.
        let mut parent_down = host(1, "10.0.0.1");
        parent_down.confirmed = Status::Down;
        assert_eq!(
            suppression_reason(&child, 0, &[], &|_| Some(parent_down.clone())),
            Some(SuppressionReason::Dependency)
        );

        // Förälder nere men avstängd: räknas inte.
        let mut parent_off = parent_down.clone();
        parent_off.enabled = false;
        assert_eq!(
            suppression_reason(&child, 0, &[], &|_| Some(parent_off.clone())),
            None
        );
    }

    #[test]
    fn saknad_forelder_tystar_inte() {
        let mut child = host(2, "10.0.0.2");
        child.depends_on_address = Some("finns.inte".to_string());
        assert_eq!(suppression_reason(&child, 0, &[], &|_| None), None);
    }

    #[test]
    fn gruppmal_traffar_ratt_enheter() {
        let mut in_group = host(1, "10.0.0.1");
        in_group.group_id = Some(7);
        let out_group = host(2, "10.0.0.2");

        let w = MaintenanceWindow {
            id: 1,
            enabled: true,
            schedule: Schedule::Once { start: 0, end: 100 },
            target: Target::Group(7),
        };

        assert!(window_targets_host(&w, &in_group));
        assert!(!window_targets_host(&w, &out_group));
    }
}
