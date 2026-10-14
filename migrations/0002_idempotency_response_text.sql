-- Store the replayable response verbatim: jsonb normalises key order, so a replay
-- would return the same JSON with its fields reordered.
ALTER TABLE idempotency_records
    ALTER COLUMN response_body TYPE text USING response_body::text;
