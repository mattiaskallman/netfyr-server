-- Indexet (address, ts) användes tidigare av statistikfrågorna men blev
-- både långsamt och mycket stort på tät historik. Statistik använder nu
-- explicit idx_samples_ts; översikten läser senaste id. Frigjorda sidor
-- återanvänds av SQLite även utan VACUUM.
DROP INDEX IF EXISTS idx_samples_addr_ts;
