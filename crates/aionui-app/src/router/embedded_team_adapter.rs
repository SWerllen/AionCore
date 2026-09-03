use std::sync::Arc;

use aionui_ai_agent::ActiveLeaseRegistry;
use aionui_api_types::ChatFileRef;
use aionui_conversation::{ConversationError, ConversationTeamOrchestrator, EmbeddedTeamSendResult};
use aionui_team::{TeamError, TeamSessionService};
use async_trait::async_trait;

pub struct EmbeddedTeamAdapter {
    service: Arc<TeamSessionService>,
    active_leases: Arc<ActiveLeaseRegistry>,
}

impl EmbeddedTeamAdapter {
    pub fn new(service: Arc<TeamSessionService>, active_leases: Arc<ActiveLeaseRegistry>) -> Self {
        Self { service, active_leases }
    }
}

fn map_team_error(error: TeamError) -> ConversationError {
    ConversationError::BadGateway {
        reason: format!("Embedded team operation failed: {error}"),
    }
}

#[async_trait]
impl ConversationTeamOrchestrator for EmbeddedTeamAdapter {
    async fn bind_leader(&self, user_id: &str, conversation_id: &str) -> Result<(), ConversationError> {
        self.service
            .ensure_embedded_team_for_conversation(user_id, conversation_id)
            .await
            .map(|_| ())
            .map_err(map_team_error)
    }

    async fn send_message(
        &self,
        user_id: &str,
        _conversation_id: &str,
        team_id: &str,
        content: &str,
        files: Vec<ChatFileRef>,
    ) -> Result<EmbeddedTeamSendResult, ConversationError> {
        let ack = self
            .service
            .send_message(user_id, team_id, content, Some(files))
            .await
            .map_err(map_team_error)?;
        Ok(EmbeddedTeamSendResult {
            message_id: ack.message_id,
            team_run_id: ack.run.team_run_id,
        })
    }

    async fn ensure_runtime(
        &self,
        user_id: &str,
        _conversation_id: &str,
        team_id: &str,
    ) -> Result<(), ConversationError> {
        self.service
            .ensure_session(user_id, team_id)
            .await
            .map_err(map_team_error)
    }

    async fn sync_leader_identity(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
    ) -> Result<(), ConversationError> {
        self.service
            .sync_embedded_leader_identity(user_id, team_id, conversation_id)
            .await
            .map_err(map_team_error)
    }

    async fn restart_runtime(
        &self,
        user_id: &str,
        _conversation_id: &str,
        team_id: &str,
        slot_id: &str,
    ) -> Result<(), ConversationError> {
        self.service
            .restart_agent_runtime(user_id, team_id, slot_id)
            .await
            .map_err(map_team_error)
    }

    async fn cancel_run(
        &self,
        user_id: &str,
        _conversation_id: &str,
        team_id: &str,
        team_run_id: &str,
    ) -> Result<(), ConversationError> {
        self.service
            .cancel_run(user_id, team_id, team_run_id, None, Some("user_requested".to_owned()))
            .await
            .map_err(map_team_error)
    }

    async fn renew_active_lease(
        &self,
        user_id: &str,
        _conversation_id: &str,
        team_id: &str,
    ) -> Result<(), ConversationError> {
        self.service
            .renew_active_lease(user_id, team_id, &self.active_leases)
            .await
            .map_err(map_team_error)
    }

    async fn remove_for_leader(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: &str,
    ) -> Result<(), ConversationError> {
        self.service
            .remove_embedded_team_for_leader(user_id, team_id, conversation_id)
            .await
            .map_err(map_team_error)
    }
}
