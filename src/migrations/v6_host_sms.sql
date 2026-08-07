-- =====================================================================
-- Schema v6: SMS-larm per enhet.
--
-- En svarslös server kan kräva omedelbar SMS-eskalering, medan en
-- svarslös smart fläkt fortfarande ska synas och loggas utan att väcka
-- jouren. DEFAULT 1 bevarar beteendet för alla befintliga enheter.
-- =====================================================================

ALTER TABLE hosts ADD COLUMN sms_enabled INTEGER NOT NULL DEFAULT 1;
