-- When the device last read HEAD before its reflog recorded the commit being made: it was made between this and `ts`.
ALTER TABLE commits ADD COLUMN not_before DOUBLE PRECISION;
