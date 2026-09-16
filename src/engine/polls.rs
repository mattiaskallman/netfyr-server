// Processlokala pollräknare för översikten.
//
// Räknarna persisteras medvetet inte: operatören ska se hur många
// kontroller den nuvarande serverprocessen har gjort. En tjänste- eller
// serveromstart skapar därför en ny, tom instans.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone, Default)]
pub struct PollCounters {
    values: Arc<RwLock<HashMap<String, (i64, i64)>>>,
}

impl PollCounters {
    pub fn record(&self, address: &str, online: bool) {
        let mut values = self.values.write().unwrap_or_else(|e| e.into_inner());
        let value = values.entry(address.to_string()).or_insert((0, 0));
        value.0 = value.0.saturating_add(1);
        if online {
            value.1 = value.1.saturating_add(1);
        }
    }

    pub fn get(&self, address: &str) -> (i64, i64) {
        self.values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(address)
            .copied()
            .unwrap_or((0, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::PollCounters;

    #[test]
    fn raknar_pollresultat_per_adress() {
        let counters = PollCounters::default();
        counters.record("10.0.0.1", true);
        counters.record("10.0.0.1", false);
        counters.record("10.0.0.2", true);

        assert_eq!(counters.get("10.0.0.1"), (2, 1));
        assert_eq!(counters.get("10.0.0.2"), (1, 1));
    }

    #[test]
    fn ny_processinstans_borjar_pa_noll() {
        let before_restart = PollCounters::default();
        before_restart.record("10.0.0.1", true);
        assert_eq!(before_restart.get("10.0.0.1"), (1, 1));

        let after_restart = PollCounters::default();
        assert_eq!(after_restart.get("10.0.0.1"), (0, 0));
    }

    #[test]
    fn kloner_delar_samma_processraknare() {
        let monitor = PollCounters::default();
        let api = monitor.clone();
        monitor.record("10.0.0.1", true);

        assert_eq!(api.get("10.0.0.1"), (1, 1));
    }
}
