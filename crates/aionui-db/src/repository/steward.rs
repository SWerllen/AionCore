use crate::error::DbError;
use crate::models::{
    StewardProfileRow, StewardReportOutboxRow, StewardTaskEventRow, StewardTaskRow, StewardTaskSessionRow,
};

#[derive(Debug, Clone, Default)]
pub struct StewardTaskFilters {
    pub query: Option<String>,
    pub lifecycle: Option<String>,
    pub limit: u32,
}

#[async_trait::async_trait]
pub trait IStewardRepository: Send + Sync {
    async fn create_task(&self, row: &StewardTaskRow) -> Result<(), DbError>;
    async fn get_task(&self, user_id: &str, task_id: &str) -> Result<Option<StewardTaskRow>, DbError>;
    async fn list_tasks(&self, user_id: &str, filters: &StewardTaskFilters) -> Result<Vec<StewardTaskRow>, DbError>;
    async fn update_task(&self, row: &StewardTaskRow) -> Result<(), DbError>;
    async fn bind_session(&self, row: &StewardTaskSessionRow) -> Result<(), DbError>;
    async fn list_sessions(&self, task_id: &str) -> Result<Vec<StewardTaskSessionRow>, DbError>;
    async fn append_event(&self, row: &StewardTaskEventRow) -> Result<(), DbError>;
    async fn list_events(&self, task_id: &str, limit: u32) -> Result<Vec<StewardTaskEventRow>, DbError>;
    async fn get_profile(&self, user_id: &str) -> Result<Option<StewardProfileRow>, DbError>;
    async fn upsert_profile(&self, row: &StewardProfileRow) -> Result<(), DbError>;
    /// Finds the durable task whose latest dispatch produced this run/turn id.
    async fn find_task_by_dispatch_run(&self, user_id: &str, run_id: &str) -> Result<Option<StewardTaskRow>, DbError>;
    /// Inserts one report per task/run. Returns false when it was already queued.
    async fn enqueue_report(&self, row: &StewardReportOutboxRow) -> Result<bool, DbError>;
    async fn list_pending_reports(&self, now: i64, limit: u32) -> Result<Vec<StewardReportOutboxRow>, DbError>;
    async fn mark_report_inbox_delivered(&self, report_id: &str, delivered_at: i64) -> Result<(), DbError>;
    async fn mark_report_im_delivered(&self, report_id: &str, delivered_at: i64) -> Result<(), DbError>;
    async fn record_report_failure(
        &self,
        report_id: &str,
        attempts: i64,
        next_attempt_at: i64,
        error: &str,
        updated_at: i64,
    ) -> Result<(), DbError>;
}
