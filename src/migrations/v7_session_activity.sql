-- Sessioner får en separat tidsstämpel för uttrycklig mänsklig aktivitet.
-- Befintliga sessioner betraktas som aktiva vid uppgraderingen, så en
-- administratör inte kastas ut direkt när tjänsten startar om.
ALTER TABLE sessions ADD COLUMN last_activity INTEGER;
UPDATE sessions
SET last_activity = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE last_activity IS NULL;
