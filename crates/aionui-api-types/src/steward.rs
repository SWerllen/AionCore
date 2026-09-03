use aionui_common::{ConversationStatus, TimestampMs};
use serde::{Deserialize, Serialize};

pub const STEWARD_MCP_SERVER_NAME: &str = "aionui-steward";
pub const DEFAULT_STEWARD_ASSISTANT_ID: &str = "bare:8e1acf31";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StewardTaskLifecycle {
    Open,
    Completed,
    Cancelled,
    Archived,
}

impl StewardTaskLifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StewardExecutionState {
    Unassigned,
    Running,
    WaitingUser,
    WaitingExternal,
    Paused,
    Interrupted,
    Failed,
    Idle,
}

impl StewardExecutionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unassigned => "unassigned",
            Self::Running => "running",
            Self::WaitingUser => "waiting_user",
            Self::WaitingExternal => "waiting_external",
            Self::Paused => "paused",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StewardSessionRole {
    Primary,
    Worker,
    Replacement,
    Observer,
}

impl StewardSessionRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Worker => "worker",
            Self::Replacement => "replacement",
            Self::Observer => "observer",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardTaskSessionResponse {
    pub id: String,
    pub conversation_id: String,
    pub conversation_name: String,
    pub role: StewardSessionRole,
    pub status: Option<ConversationStatus>,
    pub workspace: Option<String>,
    pub project_id: Option<String>,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardTaskEventResponse {
    pub id: String,
    pub source: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardTaskResponse {
    pub id: String,
    pub title: String,
    pub objective: String,
    pub lifecycle: StewardTaskLifecycle,
    pub execution_state: StewardExecutionState,
    pub priority: i64,
    pub progress_summary: Option<String>,
    pub next_action: Option<String>,
    pub blockers: Vec<String>,
    pub project_id: Option<String>,
    pub folder_id: Option<String>,
    pub workspace: Option<String>,
    pub permission_policy: serde_json::Value,
    pub budget_policy: serde_json::Value,
    pub sessions: Vec<StewardTaskSessionResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<StewardTaskEventResponse>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateStewardTaskRequest {
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub permission_policy: serde_json::Value,
    #[serde(default)]
    pub budget_policy: serde_json::Value,
    #[serde(default)]
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateStewardTaskRequest {
    pub title: Option<String>,
    pub objective: Option<String>,
    pub lifecycle: Option<StewardTaskLifecycle>,
    pub execution_state: Option<StewardExecutionState>,
    pub priority: Option<i64>,
    pub progress_summary: Option<String>,
    pub next_action: Option<String>,
    pub blockers: Option<Vec<String>>,
    pub project_id: Option<String>,
    pub folder_id: Option<String>,
    pub workspace: Option<String>,
    pub permission_policy: Option<serde_json::Value>,
    pub budget_policy: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListStewardTasksQuery {
    pub query: Option<String>,
    pub lifecycle: Option<StewardTaskLifecycle>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveStewardTaskRequest {
    pub objective: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardTaskCandidateResponse {
    pub task_id: Option<String>,
    pub conversation_id: Option<String>,
    pub title: String,
    pub score: i64,
    pub evidence: Vec<String>,
    pub lifecycle: Option<StewardTaskLifecycle>,
    pub execution_state: Option<StewardExecutionState>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BindStewardTaskSessionRequest {
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub assistant_id: Option<String>,
    #[serde(default)]
    pub conversation_name: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default = "default_session_role")]
    pub role: StewardSessionRole,
    /// Replacing an existing primary session is never implicit. Callers may
    /// enable this only after the user explicitly asks for a replacement.
    #[serde(default)]
    pub replace_primary: bool,
}

fn default_session_role() -> StewardSessionRole {
    StewardSessionRole::Primary
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResumeStewardTaskRequest {
    #[serde(default)]
    pub restart: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DispatchStewardTaskRequest {
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AskStewardTaskRequest {
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecuteStewardCommandRequest {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardCommandResponse {
    pub handled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_msg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_msg_id: Option<String>,
    pub executed_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardTaskInquiryResponse {
    pub conversation_id: String,
    pub request_msg_id: String,
    pub reply: String,
    pub replied_at: TimestampMs,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BootstrapStewardRequest {
    #[serde(default)]
    pub assistant_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SwitchStewardAssistantRequest {
    pub assistant_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardProfileResponse {
    pub conversation_id: String,
    pub assistant_id: String,
    pub conversation_name: String,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardUnregisteredConversationResponse {
    pub conversation_id: String,
    pub conversation_name: String,
    pub status: ConversationStatus,
    pub assistant_name: Option<String>,
    pub backend: Option<String>,
    pub workspace: Option<String>,
    pub project_id: Option<String>,
    pub modified_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StewardOverviewResponse {
    pub profile: Option<StewardProfileResponse>,
    pub open_tasks: usize,
    pub running_tasks: usize,
    pub waiting_tasks: usize,
    pub interrupted_tasks: usize,
    pub tasks: Vec<StewardTaskResponse>,
    #[serde(default)]
    pub unregistered_conversations: Vec<StewardUnregisteredConversationResponse>,
}
