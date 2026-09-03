use std::sync::Arc;

use crate::service::ConversationService;
use crate::steward::StewardService;
use aionui_ai_agent::{ActiveLeaseRegistry, IWorkerTaskManager};

/// Shared state for conversation route handlers.
#[derive(Clone)]
pub struct ConversationRouterState {
    pub service: ConversationService,
    pub steward: StewardService,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub active_leases: Arc<ActiveLeaseRegistry>,
}
