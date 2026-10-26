-- ywl compatibility: retain the deployed long queue and account spacing.
-- Upstream migrations 0001..0032 remain byte-for-byte unchanged.
ALTER TABLE runtime_settings
    DROP CONSTRAINT runtime_settings_concurrency_wait_timeout_seconds_check,
    ADD CONSTRAINT runtime_settings_concurrency_wait_timeout_seconds_check
        CHECK (concurrency_wait_timeout_seconds BETWEEN 1 AND 3600);
ALTER TABLE runtime_settings
    ADD COLUMN request_interval_ms bigint NOT NULL DEFAULT 50
        CHECK (request_interval_ms >= 0);
