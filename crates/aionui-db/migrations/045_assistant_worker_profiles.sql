-- User-owned execution combinations for team workers.
--
-- Profiles deliberately live outside assistant_definitions: builtin definitions
-- are global, while model/reasoning/cost choices are personal configuration.
CREATE TABLE assistant_worker_profiles (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    assistant_definition_id TEXT NOT NULL REFERENCES assistant_definitions(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    model_id TEXT NOT NULL,
    reasoning_effort TEXT,
    difficulty_ceiling INTEGER NOT NULL CHECK (difficulty_ceiling BETWEEN 1 AND 5),
    estimated_cost_micros INTEGER NOT NULL CHECK (estimated_cost_micros >= 0),
    currency TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (user_id, assistant_definition_id, name)
);

CREATE INDEX idx_assistant_worker_profiles_catalog
    ON assistant_worker_profiles (user_id, assistant_definition_id, enabled, sort_order, created_at);
