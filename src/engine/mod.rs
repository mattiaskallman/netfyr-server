// =====================================================================
// engine/mod.rs
// Övervakningsmotorn.
//
// Fyra rena moduler utan I/O, portade från desktopvariantens TypeScript
// (slow är ny i etapp 8 och finns bara på servern):
//
//   flap        tidsbaserad hysteres — när en status BEKRÄFTAS
//   suppression varför larm tystas — snooze, underhåll, beroende
//   display     härledd visningsstatus — vad som får vara rött
//   slow        latenslarmets grind — när "långsam" blir ett LARM
//
// Renheten är avsiktlig. Modulerna går att testa uttömmande utan
// databas, nätverk eller klocka, och de går att jämföra rad för rad mot
// TypeScript-originalen när något beter sig olika.
//
// Runt dem ligger fyra moduler som INTE är rena, och det är avsiktligt
// att gränsen är tydlig:
//
//   repo        databasåtkomst
//   ping        ICMP-datagramsocket
//   probe       TCP/HTTP-mätningar (etapp 8)
//   monitor     svepslingan som binder ihop allt
// =====================================================================

pub mod display;
pub mod flap;
pub mod history;
pub mod monitor;
pub mod ping;
pub mod polls;
pub mod probe;
pub mod repo;
pub mod slow;
pub mod suppression;
pub mod types;
