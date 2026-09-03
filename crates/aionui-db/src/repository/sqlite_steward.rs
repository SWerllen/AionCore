use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{
    StewardProfileRow, StewardReportOutboxRow, StewardTaskEventRow, StewardTaskRow, StewardTaskSessionRow,
};
use crate::repository::steward::{IStewardRepository, StewardTaskFilters};

#[derive(Clone, Debug)]
pub struct SqliteStewardRepository {
    pool: SqlitePool,
}

impl SqliteStewardRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IStewardRepository for SqliteStewardRepository {
    async fn create_task(&self, row: &StewardTaskRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO steward_tasks (
                id, user_id, title, objective, lifecycle, execution_state, priority,
                progress_summary, next_action, blockers, project_id, folder_id, workspace,
                permission_policy, budget_policy, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.title)
        .bind(&row.objective)
        .bind(&row.lifecycle)
        .bind(&row.execution_state)
        .bind(row.priority)
        .bind(&row.progress_summary)
        .bind(&row.next_action)
        .bind(&row.blockers)
        .bind(&row.project_id)
        .bind(&row.folder_id)
        .bind(&row.workspace)
        .bind(&row.permission_policy)
        .bind(&row.budget_policy)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_task(&self, user_id: &str, task_id: &str) -> Result<Option<StewardTaskRow>, DbError> {
        Ok(
            sqlx::query_as::<_, StewardTaskRow>("SELECT * FROM steward_tasks WHERE user_id = ? AND id = ?")
                .bind(user_id)
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn list_tasks(&self, user_id: &str, filters: &StewardTaskFilters) -> Result<Vec<StewardTaskRow>, DbError> {
        let limit = filters.limit.clamp(1, 200) as i64;
        let query = filters
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("%{}%", value.to_lowercase()));
        Ok(sqlx::query_as::<_, StewardTaskRow>(
            "SELECT * FROM steward_tasks
             WHERE user_id = ?
               AND (? IS NULL OR lifecycle = ?)
               AND (? IS NULL OR lower(title) LIKE ? OR lower(objective) LIKE ?
                    OR lower(COALESCE(progress_summary, '')) LIKE ?)
             ORDER BY CASE lifecycle WHEN 'open' THEN 0 ELSE 1 END,
                      priority DESC, updated_at DESC
             LIMIT ?",
        )
        .bind(user_id)
        .bind(&filters.lifecycle)
        .bind(&filters.lifecycle)
        .bind(&query)
        .bind(&query)
        .bind(&query)
        .bind(&query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update_task(&self, row: &StewardTaskRow) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE steward_tasks SET
                title = ?, objective = ?, lifecycle = ?, execution_state = ?, priority = ?,
                progress_summary = ?, next_action = ?, blockers = ?, project_id = ?, folder_id = ?,
                workspace = ?, permission_policy = ?, budget_policy = ?, updated_at = ?
             WHERE user_id = ? AND id = ?",
        )
        .bind(&row.title)
        .bind(&row.objective)
        .bind(&row.lifecycle)
        .bind(&row.execution_state)
        .bind(row.priority)
        .bind(&row.progress_summary)
        .bind(&row.next_action)
        .bind(&row.blockers)
        .bind(&row.project_id)
        .bind(&row.folder_id)
        .bind(&row.workspace)
        .bind(&row.permission_policy)
        .bind(&row.budget_policy)
        .bind(row.updated_at)
        .bind(&row.user_id)
        .bind(&row.id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Steward task '{}' not found", row.id)));
        }
        Ok(())
    }

    async fn bind_session(&self, row: &StewardTaskSessionRow) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        if row.role == "primary" {
            sqlx::query(
                "UPDATE steward_task_sessions SET detached_at = ?
                 WHERE task_id = ? AND role = 'primary' AND detached_at IS NULL
                   AND conversation_id <> ?",
            )
            .bind(row.created_at)
            .bind(&row.task_id)
            .bind(&row.conversation_id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO steward_task_sessions
                (id, task_id, conversation_id, role, created_at, detached_at)
             VALUES (?, ?, ?, ?, ?, NULL)
             ON CONFLICT(task_id, conversation_id) DO UPDATE SET
                role = excluded.role, detached_at = NULL",
        )
        .bind(&row.id)
        .bind(&row.task_id)
        .bind(&row.conversation_id)
        .bind(&row.role)
        .bind(row.created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn list_sessions(&self, task_id: &str) -> Result<Vec<StewardTaskSessionRow>, DbError> {
        Ok(sqlx::query_as::<_, StewardTaskSessionRow>(
            "SELECT * FROM steward_task_sessions
             WHERE task_id = ? AND detached_at IS NULL
             ORDER BY CASE role WHEN 'primary' THEN 0 ELSE 1 END, created_at ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn append_event(&self, row: &StewardTaskEventRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO steward_task_events (id, task_id, source, event_type, payload, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.task_id)
        .bind(&row.source)
        .bind(&row.event_type)
        .bind(&row.payload)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_events(&self, task_id: &str, limit: u32) -> Result<Vec<StewardTaskEventRow>, DbError> {
        Ok(sqlx::query_as::<_, StewardTaskEventRow>(
            "SELECT * FROM steward_task_events WHERE task_id = ?
             ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(task_id)
        .bind(limit.clamp(1, 200) as i64)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn get_profile(&self, user_id: &str) -> Result<Option<StewardProfileRow>, DbError> {
        Ok(
            sqlx::query_as::<_, StewardProfileRow>("SELECT * FROM steward_profiles WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn upsert_profile(&self, row: &StewardProfileRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO steward_profiles
                (user_id, conversation_id, assistant_id, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET
                conversation_id = excluded.conversation_id,
                assistant_id = excluded.assistant_id,
                updated_at = excluded.updated_at",
        )
        .bind(&row.user_id)
        .bind(&row.conversation_id)
        .bind(&row.assistant_id)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_task_by_dispatch_run(&self, user_id: &str, run_id: &str) -> Result<Option<StewardTaskRow>, DbError> {
        Ok(sqlx::query_as::<_, StewardTaskRow>(
            "SELECT t.*
             FROM steward_tasks t
             JOIN steward_task_events e ON e.task_id = t.id
             WHERE t.user_id = ?
               AND e.event_type = 'task_dispatched'
               AND json_extract(e.payload, '$.turn_id') = ?
             ORDER BY e.created_at DESC, e.id DESC
             LIMIT 1",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn enqueue_report(&self, row: &StewardReportOutboxRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO steward_report_outbox (
                id, user_id, task_id, steward_conversation_id, run_id,
                terminal_event, content, inbox_delivered_at, im_delivered_at,
                attempts, next_attempt_at, last_error, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(task_id, run_id) DO NOTHING",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.task_id)
        .bind(&row.steward_conversation_id)
        .bind(&row.run_id)
        .bind(&row.terminal_event)
        .bind(&row.content)
        .bind(row.inbox_delivered_at)
        .bind(row.im_delivered_at)
        .bind(row.attempts)
        .bind(row.next_attempt_at)
        .bind(&row.last_error)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn list_pending_reports(&self, now: i64, limit: u32) -> Result<Vec<StewardReportOutboxRow>, DbError> {
        Ok(sqlx::query_as::<_, StewardReportOutboxRow>(
            "SELECT * FROM steward_report_outbox
             WHERE next_attempt_at <= ?
               AND (inbox_delivered_at IS NULL OR im_delivered_at IS NULL)
             ORDER BY created_at ASC
             LIMIT ?",
        )
        .bind(now)
        .bind(limit.clamp(1, 100) as i64)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn mark_report_inbox_delivered(&self, report_id: &str, delivered_at: i64) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE steward_report_outbox
             SET inbox_delivered_at = COALESCE(inbox_delivered_at, ?), updated_at = ?, last_error = NULL
             WHERE id = ?",
        )
        .bind(delivered_at)
        .bind(delivered_at)
        .bind(report_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_report_im_delivered(&self, report_id: &str, delivered_at: i64) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE steward_report_outbox
             SET im_delivered_at = COALESCE(im_delivered_at, ?), updated_at = ?, last_error = NULL
             WHERE id = ?",
        )
        .bind(delivered_at)
        .bind(delivered_at)
        .bind(report_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn record_report_failure(
        &self,
        report_id: &str,
        attempts: i64,
        next_attempt_at: i64,
        error: &str,
        updated_at: i64,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE steward_report_outbox
             SET attempts = ?, next_attempt_at = ?, last_error = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(attempts)
        .bind(next_attempt_at)
        .bind(error)
        .bind(updated_at)
        .bind(report_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
