-- Durable completion reports for the personal steward.
--
-- A terminal agent/team event first becomes an outbox row.  Delivery to the
-- steward conversation and the bound IM channel is recorded independently so
-- a process restart can safely resume without creating duplicate chat rows.
CREATE TABLE steward_report_outbox (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES steward_tasks(id) ON DELETE CASCADE,
    steward_conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL,
    terminal_event TEXT NOT NULL,
    content TEXT NOT NULL,
    inbox_delivered_at INTEGER,
    im_delivered_at INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(task_id, run_id)
);

CREATE INDEX idx_steward_report_outbox_pending
    ON steward_report_outbox(next_attempt_at, created_at)
    WHERE inbox_delivered_at IS NULL OR im_delivered_at IS NULL;
