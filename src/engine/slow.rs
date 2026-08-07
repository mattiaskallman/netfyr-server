// =====================================================================
// engine/slow.rs
// Latenslarm — tillståndsmaskin (etapp 8).
//
// Tröskeln ("långsam över X ms") har funnits länge men styrde bara
// färgen i översikten. Den här modulen ger tröskeln en GRIND: svarar en
// enhet långsamt TILLRÄCKLIGT LÄNGE (slowAlarmSec) går ett larm ut, och
// ett återställningslarm när svarstiderna är normala igen.
//
// Ren modul i samma anda som flap.rs: ingen I/O, ingen databas — bara
// beslutslogik, så den går att enhetstesta utan nätverk.
//
// TILLSTÅNDET LEVER I MINNET, med flit. En omstart nollställer sviten
// och "larm skickat"-flaggan — i värsta fall går ett slow-larm ut en
// gång till efter omstart. Samma avvägning som flap-grinden: hellre
// dubbelt än uteblivet, och sviten före omstarten säger inget om läget
// efteråt.
//
// Tre skillnader mot NER-grinden, alla medvetna:
//   1. Slow-larm eskalerar ALDRIG via SMS. NER är väck-mig-nu, slow är
//      titta-vid-tillfälle. Monitorn anropar inte sms_engine::prepare
//      för slow/slow_ok.
//   2. Misslyckad mätning nollställer TYST — ingen återställning. En
//      enhet som går från långsam till helt nere får sitt NER-larm av
//      flap-grinden; ett "långsam återställd" precis före vore brus.
//   3. Ett undertryckt slow-larm prövas om varje svep så länge
//      tillståndet består (phase Pending). Återställningslarmet däremot
//      kan tappas om det undertrycks — det är ett snällt besked, inte
//      ett larm, och att hålla det kvar hade krävt persistering.
// =====================================================================

/// Vad monitorn ska göra efter att ha matat in en mätning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Inget att rapportera.
    Nothing,
    /// Sviten är lång nog — larma (om inte grinden håller tillbaka).
    /// Upprepas varje svep tills monitorn bekräftar leverans med
    /// mark_delivered().
    ShouldAlarm,
    /// Enheten svarar normalt igen efter ett levererat larm.
    Recovered,
}

/// Per-enhet tillstånd för latenslarmet.
#[derive(Debug, Clone, Copy, Default)]
pub struct SlowTracker {
    /// När den pågående långsamma sviten började. None = normal fart.
    streak_since_ms: Option<i64>,
    /// Har larmet för den här sviten levererats? Skilt från "bekräftat":
    /// ett undertryckt larm ska prövas om, inte glömmas.
    delivered: bool,
    /// Har vi redan loggat att larmet hålls tillbaka? Utan flaggan
    /// skriver monitorn samma "undertryckt"-rad varje svep.
    held_logged: bool,
}

impl SlowTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mata in en mätning.
    ///
    /// `slow` = None betyder att mätningen MISSLYCKADES — sviten
    /// nollställs tyst, NER-grinden tar över berättelsen.
    /// `slow` = Some(true/false) är en lyckad mätning över/under
    /// tröskeln.
    pub fn register(&mut self, slow: Option<bool>, now_ms: i64, alarm_sec: u32) -> Verdict {
        let Some(slow) = slow else {
            self.reset();
            return Verdict::Nothing;
        };

        if !slow {
            let recovered = self.delivered;
            self.reset();
            return if recovered {
                Verdict::Recovered
            } else {
                Verdict::Nothing
            };
        }

        // Långsam mätning. Bakåtgående klocka klampas till 0 — samma
        // eftergift som flap-grinden.
        let since = *self.streak_since_ms.get_or_insert(now_ms);
        if !self.delivered && (now_ms - since).max(0) >= i64::from(alarm_sec) * 1000 {
            return Verdict::ShouldAlarm;
        }
        Verdict::Nothing
    }

    fn reset(&mut self) {
        self.streak_since_ms = None;
        self.delivered = false;
        self.held_logged = false;
    }

    /// Monitorn anropar när larmet faktiskt är köat — inte när grinden
    /// öppnade. Ett undertryckt larm fortsätter returnera ShouldAlarm.
    pub fn mark_delivered(&mut self) {
        self.delivered = true;
    }

    /// Monitorn anropar när grinden höll tillbaka larmet. Returnerar
    /// true FÖRSTA gången per svit — bara då ska det loggas.
    pub fn mark_held(&mut self) -> bool {
        let first = !self.held_logged;
        self.held_logged = true;
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kort_sviter_larmar_inte() {
        let mut t = SlowTracker::new();
        // 59 sekunder av 60: ännu inget larm.
        assert_eq!(t.register(Some(true), 0, 60), Verdict::Nothing);
        assert_eq!(t.register(Some(true), 59_000, 60), Verdict::Nothing);
    }

    #[test]
    fn lang_svit_larmar_en_gang_per_leverans() {
        let mut t = SlowTracker::new();
        assert_eq!(t.register(Some(true), 0, 60), Verdict::Nothing);
        assert_eq!(t.register(Some(true), 61_000, 60), Verdict::ShouldAlarm);
        // Inte levererat än — grinden kan ha hållit tillbaka: pröva om.
        assert_eq!(t.register(Some(true), 71_000, 60), Verdict::ShouldAlarm);
        t.mark_delivered();
        // Levererat: tyst tills läget ändras.
        assert_eq!(t.register(Some(true), 81_000, 60), Verdict::Nothing);
    }

    #[test]
    fn aterhamtning_efter_leverans_rapporteras() {
        let mut t = SlowTracker::new();
        t.register(Some(true), 0, 60);
        t.register(Some(true), 61_000, 60);
        t.mark_delivered();
        assert_eq!(t.register(Some(false), 71_000, 60), Verdict::Recovered);
        // Tillbaka till vila: normal fart utan föregående larm är tyst.
        assert_eq!(t.register(Some(false), 72_000, 60), Verdict::Nothing);
    }

    #[test]
    fn aterhamtning_fore_tröskeln_ar_tyst() {
        let mut t = SlowTracker::new();
        t.register(Some(true), 0, 60);
        // Fladdrar tillbaka innan 60 s — inget larm gick ut, ingen
        // återställning ska heller gå ut.
        assert_eq!(t.register(Some(false), 30_000, 60), Verdict::Nothing);
    }

    #[test]
    fn misslyckad_matning_nollstaller_tyst() {
        let mut t = SlowTracker::new();
        t.register(Some(true), 0, 60);
        t.register(Some(true), 61_000, 60);
        t.mark_delivered();
        // Enheten går NER mitt i slow-larmet: ingen "återställd" — det
        // vore brus precis innan NER-larmet.
        assert_eq!(t.register(None, 71_000, 60), Verdict::Nothing);
        // När den är uppe igen börjar sviten från noll.
        assert_eq!(t.register(Some(true), 80_000, 60), Verdict::Nothing);
    }

    #[test]
    fn bakatgaende_klocka_ger_inget_paniklarm() {
        let mut t = SlowTracker::new();
        t.register(Some(true), 100_000, 60);
        assert_eq!(t.register(Some(true), 0, 60), Verdict::Nothing);
    }
}
