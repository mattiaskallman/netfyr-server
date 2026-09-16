// Bestämmer när rå historik ska sparas utan att ändra själva övervakningsfrekvensen.
// Varje statusväxling sparas direkt; ett oförändrat läge sparas högst en
// gång per minut. Pollning, larmgrind och live-status fortsätter i sina
// ordinarie intervall.

use std::collections::HashMap;

const HISTORY_INTERVAL_MS: i64 = 60_000;

#[derive(Default)]
pub struct HistoryCadence {
    last: HashMap<String, (i64, bool)>,
}

impl HistoryCadence {
    pub fn should_store(&mut self, address: &str, now_ms: i64, online: bool) -> bool {
        let store = match self.last.get(address) {
            None => true,
            Some((last_ms, last_online)) => {
                online != *last_online
                    || now_ms < *last_ms
                    || now_ms - *last_ms >= HISTORY_INTERVAL_MS
            }
        };
        if store {
            self.last.insert(address.to_string(), (now_ms, online));
        }
        store
    }
}

#[cfg(test)]
mod tests {
    use super::HistoryCadence;

    #[test]
    fn forsta_matningen_sparas() {
        let mut cadence = HistoryCadence::default();
        assert!(cadence.should_store("10.0.0.1", 1_000, true));
    }

    #[test]
    fn oforandrade_matningar_sparas_hogst_en_gang_per_minut() {
        let mut cadence = HistoryCadence::default();
        assert!(cadence.should_store("10.0.0.1", 1_000, true));
        assert!(!cadence.should_store("10.0.0.1", 59_999, true));
        assert!(cadence.should_store("10.0.0.1", 61_000, true));
    }

    #[test]
    fn statusvaxling_sparas_omedelbart() {
        let mut cadence = HistoryCadence::default();
        assert!(cadence.should_store("10.0.0.1", 1_000, true));
        assert!(cadence.should_store("10.0.0.1", 2_000, false));
        assert!(cadence.should_store("10.0.0.1", 3_000, true));
    }

    #[test]
    fn bakatgaende_klocka_startar_nytt_intervall() {
        let mut cadence = HistoryCadence::default();
        assert!(cadence.should_store("10.0.0.1", 100_000, true));
        assert!(cadence.should_store("10.0.0.1", 50_000, true));
    }
}
