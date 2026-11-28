-- Server times handed to a device after an upload and sealed into its log by a later one: events after one were made
-- after `server_ts`. `valid` is NULL when the server had no key to check the mac.
CREATE TABLE receipts (
    device_id BIGINT NOT NULL REFERENCES devices ON DELETE CASCADE,
    seq BIGINT NOT NULL,
    ts DOUBLE PRECISION NOT NULL,
    server_ts BIGINT NOT NULL,
    valid BOOLEAN,
    PRIMARY KEY (device_id, seq)
);
