use aionui_api_types::ChatFileRef;
use async_trait::async_trait;

use crate::ConversationError;

/// Minimal result shape needed by the ordinary conversation send endpoint.
///
/// The Team domain owns its richer run payload.  The conversation boundary only
/// needs stable ids so existing chat clients can keep using the ordinary
/// `SendMessageResponse` contract while an embedded team is doing the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedTeamSendResult {
    pub message_id: String,
    pub team_run_id: String,
}

/// Composition port for the latent team attached to a normal conversation.
///
/// This trait deliberately lives in the conversation domain: a normal chat must
/// not import the peer Team domain directly.  `aionui-app` supplies the adapter
/// after both services have been constructed.
#[async_trait]
pub trait ConversationTeamOrchestrator: Send + Sync {
    /// Persist a one-leader embedded team for a newly-created normal
    /// conversation. Implementations must be idempotent.
    async fn bind_leader(&self, user_id: &str, conversation_id: &str) -> Result<(), ConversationError>;

    /// Enqueue one ordinary user message through the embedded Team scheduler.
    async fn send_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
        content: &str,
        files: Vec<ChatFileRef>,
    ) -> Result<EmbeddedTeamSendResult, ConversationError>;

    /// Ensure the embedded Team session and its leader runtime are ready.
    async fn ensure_runtime(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
    ) -> Result<(), ConversationError>;

    /// Synchronize an embedded team's persisted leader identity after the
    /// ordinary conversation switches Assistant providers. Implementations
    /// must discard the old live Team session before the next attach.
    async fn sync_leader_identity(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
    ) -> Result<(), ConversationError>;

    /// Rebuild the leader runtime through the Team attach path.
    async fn restart_runtime(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
        slot_id: &str,
    ) -> Result<(), ConversationError>;

    /// Cancel the Team run represented by the ordinary conversation turn id.
    async fn cancel_run(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
        team_run_id: &str,
    ) -> Result<(), ConversationError>;

    /// Renew the lease for every member while the leader conversation is open.
    async fn renew_active_lease(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
    ) -> Result<(), ConversationError>;

    /// Remove the hidden team and worker conversations before deleting its
    /// leader. The implementation must not recursively delete the leader.
    async fn remove_for_leader(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
    ) -> Result<(), ConversationError>;
}
