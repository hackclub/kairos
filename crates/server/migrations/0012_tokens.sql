-- Token stats are opt-in: devices read and send them only while tokens_on is set.
ALTER TABLE users ADD COLUMN tokens_on BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE users ADD COLUMN tokens_public BOOLEAN NOT NULL DEFAULT FALSE;

-- Each device sends whole hours; a resend keeps the larger of each count, so logs a harness deleted never shrink them.
CREATE TABLE token_hours (
    user_id BIGINT NOT NULL REFERENCES users ON DELETE CASCADE,
    device_id BIGINT NOT NULL REFERENCES devices ON DELETE CASCADE,
    hour TIMESTAMPTZ NOT NULL,
    harness TEXT NOT NULL,
    model TEXT NOT NULL,
    project TEXT NOT NULL DEFAULT '',
    input BIGINT NOT NULL,
    output BIGINT NOT NULL,
    cache_read BIGINT NOT NULL,
    cache_write BIGINT NOT NULL,
    PRIMARY KEY (user_id, hour, device_id, harness, model, project)
);
CREATE INDEX token_hours_project ON token_hours (user_id, project, hour);

-- Dollars per token from LiteLLM's public price list, refreshed daily.
CREATE TABLE model_prices (
    model TEXT PRIMARY KEY,
    input DOUBLE PRECISION NOT NULL,
    output DOUBLE PRECISION NOT NULL,
    cache_read DOUBLE PRECISION,
    cache_write DOUBLE PRECISION,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
