------------------------------------------------------------------------
-- Native Codex / Qoder session catalogue
--
-- AionUi owns the product conversation, while the provider keeps owning the
-- actual execution history.  This table is the durable, idempotent bridge
-- between those two identities.  Rows intentionally survive conversation
-- deletion: a missing conversation_id then acts as a tombstone so a later
-- filesystem rescan does not resurrect a task the user removed in AionUi.
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS native_session_bindings (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id            TEXT    NOT NULL,
    provider           TEXT    NOT NULL,
    native_session_id  TEXT    NOT NULL,
    conversation_id    TEXT    NOT NULL,
    source_path        TEXT,
    cwd                TEXT    NOT NULL,
    project_key        TEXT    NOT NULL,
    title              TEXT    NOT NULL,
    source_updated_at  INTEGER NOT NULL,
    archived           INTEGER NOT NULL DEFAULT 0,
    imported           INTEGER NOT NULL DEFAULT 1,
    synced_at          INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_native_session_binding_identity
    ON native_session_bindings(user_id, provider, native_session_id);
CREATE INDEX IF NOT EXISTS idx_native_session_binding_conversation
    ON native_session_bindings(user_id, conversation_id);
CREATE INDEX IF NOT EXISTS idx_native_session_binding_project
    ON native_session_bindings(user_id, project_key, source_updated_at DESC);
