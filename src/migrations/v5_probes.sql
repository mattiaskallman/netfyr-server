-- =====================================================================
-- v5: probetyp per enhet (etapp 8)
--
-- Motorn kunde bara pinga. En enhet som svarar på ping men vars TJÄNST
-- ligger nere såg frisk ut — ping säger inget om att port 8080 svarar.
--
-- Två nya kolumner på hosts:
--
--   probe_type  NULL | 'icmp' | 'tcp' | 'http'
--               NULL betyder icmp — desktopens beteende och default för
--               alla enheter som fanns före etapp 8.
--   probe_port  port för tcp/http. NULL för icmp (porten är
--               betydelselös där och ska inte fylla gränssnittet med
--               skräpvärden).
--
-- Latenslarmet (slowAlarm/slowAlarmSec) behöver inga kolumner — det är
-- globala nycklar i settings och ett minnesresident tillstånd i motorn,
-- samma mönster som vakthundens respit.
-- =====================================================================

ALTER TABLE hosts ADD COLUMN probe_type TEXT;
ALTER TABLE hosts ADD COLUMN probe_port INTEGER;
