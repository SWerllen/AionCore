-- Durable user-level tasks managed by the personal steward.
-- Tasks outlive replaceable conversation runtimes and team topology changes.
CREATE TABLE steward_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    objective TEXT NOT NULL,
    lifecycle TEXT NOT NULL DEFAULT 'open'
        CHECK (lifecycle IN ('open', 'completed', 'cancelled', 'archived')),
    execution_state TEXT NOT NULL DEFAULT 'unassigned'
        CHECK (execution_state IN (
            'unassigned', 'running', 'waiting_user', 'waiting_external',
            'paused', 'interrupted', 'failed', 'idle'
        )),
    priority INTEGER NOT NULL DEFAULT 0,
    progress_summary TEXT,
    next_action TEXT,
    blockers TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(blockers)),
    project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
    folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
    workspace TEXT,
    permission_policy TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(permission_policy)),
    budget_policy TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(budget_policy)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_steward_tasks_user_lifecycle_updated
    ON steward_tasks (user_id, lifecycle, updated_at DESC);
CREATE INDEX idx_steward_tasks_project
    ON steward_tasks (user_id, project_id, updated_at DESC);

CREATE TABLE steward_task_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES steward_tasks(id) ON DELETE CASCADE,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role TEXT NOT NULL DEFAULT 'primary'
        CHECK (role IN ('primary', 'worker', 'replacement', 'observer')),
    created_at INTEGER NOT NULL,
    detached_at INTEGER,
    UNIQUE (task_id, conversation_id)
);

CREATE UNIQUE INDEX idx_steward_task_one_active_primary
    ON steward_task_sessions (task_id)
    WHERE role = 'primary' AND detached_at IS NULL;
CREATE INDEX idx_steward_task_sessions_conversation
    ON steward_task_sessions (conversation_id, detached_at);

CREATE TABLE steward_task_events (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES steward_tasks(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload)),
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_steward_task_events_task_created
    ON steward_task_events (task_id, created_at DESC);

CREATE TABLE steward_profiles (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    conversation_id TEXT UNIQUE REFERENCES conversations(id) ON DELETE SET NULL,
    assistant_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
