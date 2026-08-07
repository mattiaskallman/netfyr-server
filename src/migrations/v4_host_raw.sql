-- =====================================================================
-- v4: råstatus i host_status
--
-- Motorn räknar ut det råa utfallet (online/warning/offline) varje svep
-- men sparade det aldrig. Översikten fick då gissa råläget ur senaste
-- svarstiden — och gissade fel i båda riktningarna:
--
--   * missad ping → svarstid NULL → gissningen "online". Enheten visades
--     grön ända tills grinden bekräftat NER, utan mellanläget "osäker".
--   * bekräftat NER → gissningen tvang "offline" även när svaren börjat
--     komma tillbaka, så "återhämtar" kunde aldrig visas.
--
-- Med råstatusen lagrad behöver översikten inte gissa alls.
-- =====================================================================

ALTER TABLE host_status ADD COLUMN raw TEXT;
