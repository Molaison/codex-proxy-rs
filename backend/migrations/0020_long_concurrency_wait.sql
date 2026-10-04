-- 长任务排队需要超过两分钟；执行并发和等待人数上限保持原有约束。
ALTER TABLE runtime_settings
    DROP CONSTRAINT runtime_settings_concurrency_wait_timeout_seconds_check,
    ADD CONSTRAINT runtime_settings_concurrency_wait_timeout_seconds_check
        CHECK (concurrency_wait_timeout_seconds BETWEEN 1 AND 3600);
