-- Generate /tmp/sof-native.parquet from examples/temporal_view.json first.
SET TimeZone = 'UTC';
CREATE VIEW native AS SELECT * FROM read_parquet('/tmp/sof-native.parquet');
CREATE VIEW expected AS SELECT * FROM (VALUES
    ('synthetic-1', DATE '2000-02-01', TIMESTAMPTZ '2026-03-01 00:00:00+00', TIMESTAMPTZ '2026-03-01 10:00:00.123456+00', TIME '09:30:00'),
    ('synthetic-2', DATE '1980-01-01', TIMESTAMPTZ '2026-03-02 08:00:00+00', TIMESTAMPTZ '2026-03-02 08:00:00+00', TIME '10:15:00.250')
) AS rows(id, birth_date, deceased_at, last_updated, preferred_time);
SELECT CASE WHEN
    (SELECT count(*) FROM native) = 2
    AND NOT EXISTS (SELECT * FROM expected EXCEPT ALL SELECT * FROM native)
    AND (SELECT bool_and(
        typeof(birth_date) = 'DATE'
        AND typeof(deceased_at) = 'TIMESTAMP WITH TIME ZONE'
        AND typeof(last_updated) = 'TIMESTAMP WITH TIME ZONE'
        AND typeof(preferred_time) = 'TIME'
    ) FROM native)
THEN 'Native temporal schema and values match'
ELSE error('Native Parquet interoperability regression') END;
