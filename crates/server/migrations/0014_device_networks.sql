-- Where a device's uploads come from, one row per change: provider (ASN) and country looked up on the server, a keyed
-- hash of the address (never the address), a fingerprint of the request headers and how far off the device's clock is.
CREATE TABLE device_networks (
    device_id BIGINT NOT NULL REFERENCES devices ON DELETE CASCADE,
    since DOUBLE PRECISION NOT NULL,
    last_seen DOUBLE PRECISION NOT NULL,
    asn BIGINT,
    provider TEXT,
    country TEXT,
    ip_hash TEXT,
    fingerprint TEXT NOT NULL,
    clock_skew DOUBLE PRECISION,
    PRIMARY KEY (device_id, since)
);
CREATE INDEX device_networks_ip_hash ON device_networks (ip_hash) WHERE ip_hash IS NOT NULL;
