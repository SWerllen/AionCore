-- Optional Qoder-native context-window override for reusable worker profiles.
-- NULL means the selected model/provider keeps its own default.
ALTER TABLE assistant_worker_profiles
    ADD COLUMN context_window INTEGER
    CHECK (context_window IS NULL OR context_window > 0);
