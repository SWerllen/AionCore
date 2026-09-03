use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct StewardTaskRow {
    pub id: String,
    pub user_id: String,
    pub title: String,
    pub objective: String,
    pub lifecycle: String,
    pub execution_state: String,
    pub priority: i64,
    pub progress_summary: Option<String>,
    pub next_action: Option<String>,
    pub blockers: String,
    pub project_id: Option<String>,
    pub folder_id: Option<String>,
    pub workspace: Option<String>,
    pub permission_policy: String,
    pub budget_policy: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct StewardTaskSessionRow {
    pub id: String,
    pub task_id: String,
    pub conversation_id: String,
    pub role: String,
    pub created_at: TimestampMs,
    pub detached_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct StewardTaskEventRow {
    pub id: String,
    pub task_id: String,
    pub source: String,
    pub event_type: String,
    pub payload: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct StewardProfileRow {
    pub user_id: String,
    pub conversation_id: Option<String>,
    pub assistant_id: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct StewardReportOutboxRow {
    pub id: String,
    pub user_id: String,
    pub task_id: String,
    pub steward_conversation_id: String,
    pub run_id: String,
    pub terminal_event: String,
    pub content: String,
    pub inbox_delivered_at: Option<TimestampMs>,
    pub im_delivered_at: Option<TimestampMs>,
    pub attempts: i64,
    pub next_attempt_at: TimestampMs,
    pub last_error: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
