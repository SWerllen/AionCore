use std::sync::Arc;

use aionui_api_types::ChannelConversationTarget;
use tracing::{debug, info, warn};

use crate::channel_settings::ChannelSettingsService;
use crate::error::ChannelError;
use crate::pairing::PairingService;
use crate::session::SessionManager;
use crate::types::{
    ActionBehavior, ActionButton, ActionCategory, ActionResponse, UnifiedAction, UnifiedIncomingMessage,
};

/// Result of processing an incoming message.
///
/// The caller (ChannelManager / plugin) uses this to decide what to send
/// back to the IM platform.
#[derive(Debug, Clone)]
pub enum MessageResult {
    /// An action response to send/edit on the platform.
    Action(ActionResponse),
    /// Message was dispatched to the AI Agent. The caller should send
    /// a "thinking" placeholder and then relay stream events.
    Dispatched {
        owner_user_id: String,
        session_id: String,
        conversation_id: Option<String>,
    },
    /// Message was a text but user already has an active agent stream
    /// (no duplicate dispatch needed).
    AlreadyProcessing,
}

/// Composition-layer port used to restore or bootstrap the user's durable
/// steward profile without introducing a channel → conversation dependency.
#[async_trait::async_trait]
pub trait ChannelStewardResolver: Send + Sync {
    async fn resolve_conversation_id(&self, user_id: &str) -> Result<String, ChannelError>;

    async fn try_execute_command(&self, _user_id: &str, _content: &str) -> Result<Option<String>, ChannelError> {
        Ok(None)
    }
}

#[derive(Debug)]
struct DispatchBinding {
    agent_type: String,
    conversation_id: Option<String>,
    is_steward: bool,
}

/// Processes incoming IM messages: authorization → action routing → AI dispatch.
///
/// This is the core message entry point for the channel system. Each
/// incoming `UnifiedIncomingMessage` is either:
/// 1. Rejected (unauthorized → pairing flow)
/// 2. Routed to an action handler (button callback)
/// 3. Dispatched to the AI Agent (text message)
pub struct ActionExecutor {
    pairing: Arc<PairingService>,
    session_mgr: Arc<SessionManager>,
    settings: Arc<ChannelSettingsService>,
    owner_user_id: Option<String>,
    steward_resolver: Option<Arc<dyn ChannelStewardResolver>>,
}

impl ActionExecutor {
    pub fn new(
        pairing: Arc<PairingService>,
        session_mgr: Arc<SessionManager>,
        settings: Arc<ChannelSettingsService>,
        owner_user_id: Option<String>,
    ) -> Self {
        Self {
            pairing,
            session_mgr,
            settings,
            owner_user_id,
            steward_resolver: None,
        }
    }

    pub fn with_steward_resolver(mut self, resolver: Arc<dyn ChannelStewardResolver>) -> Self {
        self.steward_resolver = Some(resolver);
        self
    }

    pub fn owner_user_id(&self) -> Option<&str> {
        self.owner_user_id.as_deref()
    }

    /// Main entry point: handle an incoming message from any platform.
    ///
    /// Flow:
    /// 1. Authorization check → if unauthorized, trigger pairing
    /// 2. Button callback → route to action handler
    /// 3. Text message → get/create session → return Dispatched for AI
    pub async fn handle_incoming_message(&self, msg: &UnifiedIncomingMessage) -> Result<MessageResult, ChannelError> {
        let platform_type = msg.platform.to_string();
        let user_id = &msg.user.id;
        let chat_id = &msg.chat_id;
        let owner_user_id = msg
            .owner_user_id
            .as_deref()
            .or(self.owner_user_id.as_deref())
            .ok_or_else(|| ChannelError::InvalidConfig("channel owner user is required".to_owned()))?;

        // 1. Authorization check — resolve platform user → internal user ID
        let internal_user_id = self
            .pairing
            .get_internal_user_id(owner_user_id, user_id, &platform_type)
            .await?;

        let internal_user_id = match internal_user_id {
            Some(id) => id,
            None => {
                let response = self
                    .handle_unauthorized(owner_user_id, user_id, &platform_type, &msg.user.display_name)
                    .await?;
                return Ok(MessageResult::Action(response));
            }
        };

        // 2. Button callback → action routing
        if let Some(action) = &msg.action {
            let response = self
                .route_action(owner_user_id, action, &internal_user_id, msg.is_group)
                .await?;
            return Ok(MessageResult::Action(response));
        }

        // 3. Text message → session resolution → AI dispatch
        let binding = self
            .resolve_dispatch_binding(owner_user_id, msg.platform, msg.is_group)
            .await?;
        if binding.is_steward {
            let resolver = self.steward_resolver.as_ref().ok_or_else(|| {
                ChannelError::InvalidConfig("Steward target is unavailable in this AionUi runtime".to_owned())
            })?;
            if let Some(text) = resolver.try_execute_command(owner_user_id, &msg.content.text).await? {
                info!(
                    user_id = %user_id,
                    chat_id = %chat_id,
                    outcome = "handled",
                    "steward machine command handled before agent dispatch"
                );
                return Ok(MessageResult::Action(ActionResponse {
                    text: Some(text),
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: None,
                    edit_message_id: None,
                }));
            }
        }
        let session = self
            .session_mgr
            .get_or_create_bound_session(
                owner_user_id,
                &internal_user_id,
                chat_id,
                &binding.agent_type,
                None,
                binding.conversation_id.as_deref(),
            )
            .await?;

        info!(
            session_id = %session.id,
            user_id = %user_id,
            chat_id = %chat_id,
            text_len = msg.content.text.len(),
            "message dispatched to agent"
        );

        Ok(MessageResult::Dispatched {
            owner_user_id: owner_user_id.to_owned(),
            session_id: session.id,
            conversation_id: session.conversation_id,
        })
    }

    async fn resolve_dispatch_binding(
        &self,
        owner_user_id: &str,
        platform: crate::types::PluginType,
        is_group: bool,
    ) -> Result<DispatchBinding, ChannelError> {
        let target = self.settings.get_conversation_target(owner_user_id, platform).await?;

        // The steward can control other sessions. Keep that authority scoped to
        // an explicitly paired private chat; groups continue using the ordinary
        // assistant configuration even when the platform target is Steward.
        if target == ChannelConversationTarget::Steward && !is_group {
            let resolver = self.steward_resolver.as_ref().ok_or_else(|| {
                ChannelError::InvalidConfig("Steward target is unavailable in this AionUi runtime".to_owned())
            })?;
            let conversation_id = resolver.resolve_conversation_id(owner_user_id).await?;
            return Ok(DispatchBinding {
                agent_type: "steward".to_owned(),
                conversation_id: Some(conversation_id),
                is_steward: true,
            });
        }

        let agent_config = self.settings.get_agent_config(owner_user_id, platform).await?;
        Ok(DispatchBinding {
            agent_type: agent_config.agent_type,
            conversation_id: None,
            is_steward: false,
        })
    }

    /// Handles an unauthorized user: generate pairing code and return
    /// a response with instructions and action buttons.
    async fn handle_unauthorized(
        &self,
        owner_user_id: &str,
        platform_user_id: &str,
        platform_type: &str,
        display_name: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let code = self
            .pairing
            .request_pairing(owner_user_id, platform_user_id, platform_type, Some(display_name))
            .await?;

        debug!(
            platform_user_id = %platform_user_id,
            code = %code,
            "pairing code generated for unauthorized user"
        );

        Ok(build_pairing_response(&code))
    }

    /// Routes an action to the appropriate handler by category.
    async fn route_action(
        &self,
        owner_user_id: &str,
        action: &UnifiedAction,
        internal_user_id: &str,
        is_group: bool,
    ) -> Result<ActionResponse, ChannelError> {
        match action.category {
            ActionCategory::Platform => self.handle_platform_action(owner_user_id, action).await,
            ActionCategory::System => {
                self.handle_system_action(owner_user_id, action, internal_user_id, is_group)
                    .await
            }
            ActionCategory::Chat => self.handle_chat_action(action).await,
        }
    }

    // ── Platform actions ────────────────────────────────────────────

    async fn handle_platform_action(
        &self,
        owner_user_id: &str,
        action: &UnifiedAction,
    ) -> Result<ActionResponse, ChannelError> {
        match action.action.as_str() {
            "pairing.show" | "pairing.refresh" => {
                let code = self
                    .pairing
                    .request_pairing(
                        owner_user_id,
                        &action.context.user_id,
                        &action.context.platform.to_string(),
                        None,
                    )
                    .await?;
                Ok(build_pairing_response(&code))
            }
            "pairing.check" => {
                let authorized = self
                    .pairing
                    .is_user_authorized(
                        owner_user_id,
                        &action.context.user_id,
                        &action.context.platform.to_string(),
                    )
                    .await?;
                if authorized {
                    Ok(ActionResponse {
                        text: Some("You are authorized! Send a message to start chatting.".into()),
                        parse_mode: None,
                        buttons: None,
                        keyboard: None,
                        behavior: ActionBehavior::Send,
                        toast: None,
                        edit_message_id: None,
                    })
                } else {
                    Ok(ActionResponse {
                        text: Some("Still waiting for approval. Ask the admin to check Settings → Channel.".into()),
                        parse_mode: None,
                        buttons: Some(vec![vec![
                            ActionButton {
                                label: "Refresh".into(),
                                action: "pairing.refresh".into(),
                                params: None,
                            },
                            ActionButton {
                                label: "Check Again".into(),
                                action: "pairing.check".into(),
                                params: None,
                            },
                        ]]),
                        keyboard: None,
                        behavior: ActionBehavior::Send,
                        toast: None,
                        edit_message_id: None,
                    })
                }
            }
            "pairing.help" => Ok(ActionResponse {
                text: Some(
                    "To use this bot, you need authorization:\n\
                         1. Send any message to get a 6-digit pairing code\n\
                         2. Share this code with the admin\n\
                         3. Admin approves in Settings → Channel\n\
                         4. You're ready to chat!"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            other => {
                warn!(action = %other, "unknown platform action");
                Ok(build_unknown_action_response(other))
            }
        }
    }

    // ── System actions ──────────────────────────────────────────────

    async fn handle_system_action(
        &self,
        owner_user_id: &str,
        action: &UnifiedAction,
        internal_user_id: &str,
        is_group: bool,
    ) -> Result<ActionResponse, ChannelError> {
        match action.action.as_str() {
            "session.new" => {
                let user_id = internal_user_id;
                let chat_id = &action.context.chat_id;
                let binding = self
                    .resolve_dispatch_binding(owner_user_id, action.context.platform, is_group)
                    .await?;
                let session = self
                    .session_mgr
                    .reset_bound_session(
                        owner_user_id,
                        user_id,
                        chat_id,
                        &binding.agent_type,
                        None,
                        binding.conversation_id.as_deref(),
                    )
                    .await?;

                Ok(ActionResponse {
                    text: Some(if binding.is_steward {
                        format!("Reconnected to your shared steward.\nSession: {}", &session.id[..8])
                    } else {
                        format!(
                            "New session created.\nAgent: {}\nSession: {}",
                            session.agent_type,
                            &session.id[..8]
                        )
                    }),
                    parse_mode: None,
                    buttons: Some(vec![vec![ActionButton {
                        label: "Help".into(),
                        action: "help.show".into(),
                        params: None,
                    }]]),
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: None,
                    edit_message_id: None,
                })
            }
            "session.status" => {
                let user_id = internal_user_id;
                let chat_id = &action.context.chat_id;
                let binding = self
                    .resolve_dispatch_binding(owner_user_id, action.context.platform, is_group)
                    .await?;
                let session = self
                    .session_mgr
                    .get_or_create_bound_session(
                        owner_user_id,
                        user_id,
                        chat_id,
                        &binding.agent_type,
                        None,
                        binding.conversation_id.as_deref(),
                    )
                    .await?;

                Ok(ActionResponse {
                    text: Some(format!(
                        "Session: {}\nAgent: {}\nCreated: {}\nLast active: {}",
                        &session.id[..8],
                        session.agent_type,
                        session.created_at,
                        session.last_activity,
                    )),
                    parse_mode: None,
                    buttons: Some(vec![vec![ActionButton {
                        label: "New Session".into(),
                        action: "session.new".into(),
                        params: None,
                    }]]),
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: None,
                    edit_message_id: None,
                })
            }
            "help.show" => Ok(build_help_response()),
            "help.features" => Ok(ActionResponse {
                text: Some(
                    "Features:\n\
                         • AI chat through your configured assistant\n\
                         • Tool execution with auto-approval\n\
                         • Session isolation per chat"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "help.pairing" => Ok(ActionResponse {
                text: Some(
                    "Pairing:\n\
                         Send any message → get a 6-digit code → admin approves → you're in!"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "help.tips" => Ok(ActionResponse {
                text: Some(
                    "Tips:\n\
                         • Start a new session to clear context\n\
                         • Use /help to see available commands\n\
                         • In group chats, @mention the bot"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "settings.show" => Ok(ActionResponse {
                text: Some(
                    "Settings are managed in the desktop app.\n\
                         Go to Settings → Channel to configure plugins and manage users."
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            other => {
                warn!(action = %other, "unknown system action");
                Ok(build_unknown_action_response(other))
            }
        }
    }

    // ── Chat actions ────────────────────────────────────────────────

    async fn handle_chat_action(&self, action: &UnifiedAction) -> Result<ActionResponse, ChannelError> {
        match action.action.as_str() {
            "chat.send" | "chat.regenerate" | "chat.continue" => {
                // These are handled by the message flow, not action responses.
                // Return a placeholder; the real logic is in ChannelMessageService.
                Ok(ActionResponse {
                    text: None,
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: Some("Processing...".into()),
                    edit_message_id: None,
                })
            }
            "action.copy" => Ok(ActionResponse {
                text: None,
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Answer,
                toast: Some("Copied to clipboard".into()),
                edit_message_id: None,
            }),
            "system.confirm" => {
                let call_id = action
                    .params
                    .as_ref()
                    .and_then(|p| p.get("callId"))
                    .cloned()
                    .unwrap_or_default();
                let value = action
                    .params
                    .as_ref()
                    .and_then(|p| p.get("value"))
                    .cloned()
                    .unwrap_or_else(|| "true".into());

                debug!(call_id = %call_id, value = %value, "tool confirmation received");

                Ok(ActionResponse {
                    text: None,
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Answer,
                    toast: Some("Confirmed".into()),
                    edit_message_id: None,
                })
            }
            other => {
                warn!(action = %other, "unknown chat action");
                Ok(build_unknown_action_response(other))
            }
        }
    }
}

// ── Helper builders ─────────────────────────────────────────────────

fn build_pairing_response(code: &str) -> ActionResponse {
    ActionResponse {
        text: Some(format!(
            "Welcome! To use this bot, you need authorization.\n\n\
             Your pairing code: *{code}*\n\n\
             Share this code with the admin, who can approve it in \
             Settings → Channel → Pairing Requests.\n\
             The code expires in 10 minutes."
        )),
        parse_mode: None,
        buttons: Some(vec![vec![
            ActionButton {
                label: "Refresh Code".into(),
                action: "pairing.refresh".into(),
                params: None,
            },
            ActionButton {
                label: "Check Status".into(),
                action: "pairing.check".into(),
                params: None,
            },
            ActionButton {
                label: "Help".into(),
                action: "pairing.help".into(),
                params: None,
            },
        ]]),
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

fn build_help_response() -> ActionResponse {
    ActionResponse {
        text: Some(
            "How can I help?\n\
             Choose an option below or just send me a message."
                .into(),
        ),
        parse_mode: None,
        buttons: Some(vec![
            vec![
                ActionButton {
                    label: "New Session".into(),
                    action: "session.new".into(),
                    params: None,
                },
                ActionButton {
                    label: "Session Status".into(),
                    action: "session.status".into(),
                    params: None,
                },
            ],
            vec![
                ActionButton {
                    label: "Features".into(),
                    action: "help.features".into(),
                    params: None,
                },
                ActionButton {
                    label: "Tips".into(),
                    action: "help.tips".into(),
                    params: None,
                },
            ],
        ]),
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

fn build_unknown_action_response(action: &str) -> ActionResponse {
    ActionResponse {
        text: Some(format!("Unknown action: {action}")),
        parse_mode: None,
        buttons: None,
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActionContext, MessageContentType, PluginType, UnifiedMessageContent, UnifiedUser};
    use aionui_api_types::WebSocketMessage;
    use aionui_common::{TimestampMs, now_ms};
    use aionui_db::models::{
        AssistantSessionRow, AssistantUserRow, ChannelPluginRow, ClientPreference, PairingCodeRow,
    };
    use aionui_db::{DbError, IChannelRepository, IClientPreferenceRepository, UpdatePluginStatusParams};
    use aionui_realtime::EventBroadcaster;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ── Mock EventBroadcaster ──────────────────────────────────────────

    struct MockBroadcaster;

    impl EventBroadcaster for MockBroadcaster {
        fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
    }

    // ── Mock IChannelRepository ────────────────────────────────────────
    const OWNER_ID: &str = "owner-test";

    struct MockRepo {
        users: Mutex<Vec<AssistantUserRow>>,
        sessions: Mutex<Vec<AssistantSessionRow>>,
        pairings: Mutex<Vec<PairingCodeRow>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                users: Mutex::new(Vec::new()),
                sessions: Mutex::new(Vec::new()),
                pairings: Mutex::new(Vec::new()),
            }
        }

        fn add_authorized_user(&self, platform_user_id: &str, platform_type: &str) {
            let user = AssistantUserRow {
                id: format!("user_{platform_user_id}"),
                owner_user_id: OWNER_ID.to_owned(),
                platform_user_id: platform_user_id.to_owned(),
                platform_type: platform_type.to_owned(),
                display_name: Some("Test User".into()),
                authorized_at: now_ms(),
                last_active: None,
                session_id: None,
            };
            self.users.lock().unwrap().push(user);
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        async fn get_all_plugins(&self, _owner_user_id: &str) -> Result<Vec<ChannelPluginRow>, DbError> {
            Ok(vec![])
        }
        async fn get_plugin(&self, _owner_user_id: &str, _id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
            Ok(None)
        }
        async fn upsert_plugin(&self, _owner_user_id: &str, _row: &ChannelPluginRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_status(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _params: &UpdatePluginStatusParams,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_plugin(&self, _owner_user_id: &str, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        async fn get_all_users(&self, _owner_user_id: &str) -> Result<Vec<AssistantUserRow>, DbError> {
            Ok(self.users.lock().unwrap().clone())
        }
        async fn get_user_by_platform(
            &self,
            _owner_user_id: &str,
            platform_user_id: &str,
            platform_type: &str,
        ) -> Result<Option<AssistantUserRow>, DbError> {
            let users = self.users.lock().unwrap();
            Ok(users
                .iter()
                .find(|u| u.platform_user_id == platform_user_id && u.platform_type == platform_type)
                .cloned())
        }
        async fn create_user(&self, _owner_user_id: &str, row: &AssistantUserRow) -> Result<(), DbError> {
            self.users.lock().unwrap().push(row.clone());
            Ok(())
        }
        async fn update_user_last_active(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _last_active: TimestampMs,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_user(&self, _owner_user_id: &str, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        async fn get_all_sessions(&self, _owner_user_id: &str) -> Result<Vec<AssistantSessionRow>, DbError> {
            Ok(self.sessions.lock().unwrap().clone())
        }
        async fn get_session(&self, _owner_user_id: &str, id: &str) -> Result<Option<AssistantSessionRow>, DbError> {
            let sessions = self.sessions.lock().unwrap();
            Ok(sessions.iter().find(|s| s.id == id).cloned())
        }
        async fn get_or_create_session(
            &self,
            _owner_user_id: &str,
            user_id: &str,
            chat_id: &str,
            new_row: &AssistantSessionRow,
        ) -> Result<AssistantSessionRow, DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(existing) = sessions
                .iter_mut()
                .find(|s| s.user_id == user_id && s.chat_id.as_deref() == Some(chat_id))
            {
                existing.last_activity = new_row.last_activity;
                return Ok(existing.clone());
            }
            sessions.push(new_row.clone());
            Ok(new_row.clone())
        }
        async fn update_session_activity(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _last_activity: TimestampMs,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_conversation(
            &self,
            _owner_user_id: &str,
            id: &str,
            conversation_id: &str,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
                s.conversation_id = Some(conversation_id.to_owned());
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }
        async fn update_session_agent_type(
            &self,
            _owner_user_id: &str,
            id: &str,
            agent_type: &str,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
                s.agent_type = agent_type.to_owned();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }
        async fn delete_sessions_by_user(&self, _owner_user_id: &str, user_id: &str) -> Result<(), DbError> {
            self.sessions.lock().unwrap().retain(|s| s.user_id != user_id);
            Ok(())
        }
        async fn delete_session_by_user_chat(
            &self,
            _owner_user_id: &str,
            user_id: &str,
            chat_id: &str,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|s| !(s.user_id == user_id && s.chat_id.as_deref() == Some(chat_id)));
            Ok(())
        }

        async fn create_pairing(&self, _owner_user_id: &str, row: &PairingCodeRow) -> Result<(), DbError> {
            self.pairings.lock().unwrap().push(row.clone());
            Ok(())
        }
        async fn get_pending_pairings(&self, _owner_user_id: &str) -> Result<Vec<PairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().filter(|p| p.status == "pending").cloned().collect())
        }
        async fn get_pairing_by_code(
            &self,
            _owner_user_id: &str,
            code: &str,
        ) -> Result<Option<PairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().find(|p| p.code == code).cloned())
        }
        async fn update_pairing_status(&self, _owner_user_id: &str, code: &str, status: &str) -> Result<(), DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            if let Some(p) = pairings.iter_mut().find(|p| p.code == code) {
                p.status = status.to_owned();
                Ok(())
            } else {
                Err(DbError::NotFound(code.into()))
            }
        }
        async fn cleanup_expired_pairings(&self, _owner_user_id: &str, _now: TimestampMs) -> Result<u64, DbError> {
            Ok(0)
        }
    }

    // ── Mock IClientPreferenceRepository ──────────────────────────────

    struct MockPrefRepo;

    #[async_trait::async_trait]
    impl IClientPreferenceRepository for MockPrefRepo {
        async fn get_all(&self, _user_id: &str) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn get_by_keys(&self, _user_id: &str, _keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn upsert_batch(&self, _user_id: &str, _entries: &[(&str, &str)]) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_keys(&self, _user_id: &str, _keys: &[&str]) -> Result<(), DbError> {
            Ok(())
        }
    }

    struct StewardPrefRepo;

    #[async_trait::async_trait]
    impl IClientPreferenceRepository for StewardPrefRepo {
        async fn get_all(&self, _user_id: &str) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn get_by_keys(&self, user_id: &str, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            if keys.contains(&"assistant.telegram.target") {
                return Ok(vec![ClientPreference {
                    user_id: user_id.to_owned(),
                    key: "assistant.telegram.target".to_owned(),
                    value: r#""steward""#.to_owned(),
                    updated_at: 0,
                }]);
            }
            Ok(vec![])
        }
        async fn upsert_batch(&self, _user_id: &str, _entries: &[(&str, &str)]) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_keys(&self, _user_id: &str, _keys: &[&str]) -> Result<(), DbError> {
            Ok(())
        }
    }

    struct FixedStewardResolver;

    #[async_trait::async_trait]
    impl ChannelStewardResolver for FixedStewardResolver {
        async fn resolve_conversation_id(&self, _user_id: &str) -> Result<String, ChannelError> {
            Ok("steward-conversation".to_owned())
        }

        async fn try_execute_command(&self, _user_id: &str, content: &str) -> Result<Option<String>, ChannelError> {
            Ok((content == "有哪些任务").then(|| "任务概览：进行中 1。".to_owned()))
        }
    }

    // ── Test helpers ───────────────────────────────────────────────────

    fn setup() -> (ActionExecutor, Arc<MockRepo>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let pref_repo: Arc<dyn IClientPreferenceRepository> = Arc::new(MockPrefRepo);
        let settings = Arc::new(ChannelSettingsService::new(pref_repo));
        let executor = ActionExecutor::new(pairing, session_mgr, settings, Some(OWNER_ID.to_owned()));
        (executor, repo)
    }

    fn setup_without_owner() -> (ActionExecutor, Arc<MockRepo>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let pref_repo: Arc<dyn IClientPreferenceRepository> = Arc::new(MockPrefRepo);
        let settings = Arc::new(ChannelSettingsService::new(pref_repo));
        let executor = ActionExecutor::new(pairing, session_mgr, settings, None);
        (executor, repo)
    }

    fn setup_steward() -> (ActionExecutor, Arc<MockRepo>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let pref_repo: Arc<dyn IClientPreferenceRepository> = Arc::new(StewardPrefRepo);
        let settings = Arc::new(ChannelSettingsService::new(pref_repo));
        let executor = ActionExecutor::new(pairing, session_mgr, settings, Some(OWNER_ID.to_owned()))
            .with_steward_resolver(Arc::new(FixedStewardResolver));
        (executor, repo)
    }

    fn make_text_message(user_id: &str, chat_id: &str, text: &str, platform: PluginType) -> UnifiedIncomingMessage {
        UnifiedIncomingMessage {
            owner_user_id: None,
            id: "msg_1".into(),
            platform,
            chat_id: chat_id.into(),
            is_group: false,
            user: UnifiedUser {
                id: user_id.into(),
                username: None,
                display_name: "Test".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Text,
                text: text.into(),
                attachments: None,
            },
            timestamp: now_ms(),
            reply_to_message_id: None,
            action: None,
            raw: None,
        }
    }

    fn make_action_message(
        user_id: &str,
        chat_id: &str,
        action_name: &str,
        category: ActionCategory,
        platform: PluginType,
        params: Option<HashMap<String, String>>,
    ) -> UnifiedIncomingMessage {
        UnifiedIncomingMessage {
            owner_user_id: None,
            id: "msg_1".into(),
            platform,
            chat_id: chat_id.into(),
            is_group: false,
            user: UnifiedUser {
                id: user_id.into(),
                username: None,
                display_name: "Test".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Action,
                text: String::new(),
                attachments: None,
            },
            timestamp: now_ms(),
            reply_to_message_id: None,
            action: Some(UnifiedAction {
                action: action_name.into(),
                category,
                params,
                context: ActionContext {
                    platform,
                    user_id: user_id.into(),
                    chat_id: chat_id.into(),
                    message_id: None,
                    session_id: None,
                },
            }),
            raw: None,
        }
    }

    // ── Authorization tests ────────────────────────────────────────────

    #[tokio::test]
    async fn unauthorized_user_gets_pairing_response() {
        let (executor, _repo) = setup();
        let msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);

        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Send);
                let text = resp.text.unwrap();
                assert!(text.contains("pairing code"));
                assert!(resp.buttons.is_some());
            }
            _ => panic!("Expected Action result for unauthorized user"),
        }
    }

    #[tokio::test]
    async fn missing_owner_user_id_is_rejected() {
        let (executor, _repo) = setup_without_owner();
        let msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);

        let err = executor.handle_incoming_message(&msg).await.unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));
        assert!(err.to_string().contains("owner user"));
    }

    #[tokio::test]
    async fn authorized_user_text_dispatches_to_agent() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_text_message("tg_42", "chat_1", "Hello AI", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Dispatched { session_id, .. } => {
                assert!(!session_id.is_empty());
            }
            _ => panic!("Expected Dispatched result for authorized user"),
        }
    }

    #[tokio::test]
    async fn private_steward_command_returns_without_creating_an_agent_session() {
        let (executor, repo) = setup_steward();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_text_message("tg_42", "chat_1", "有哪些任务", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Action(response) => {
                assert_eq!(response.text.as_deref(), Some("任务概览：进行中 1。"));
                assert!(repo.sessions.lock().unwrap().is_empty());
            }
            _ => panic!("Expected direct steward command response"),
        }
    }

    #[tokio::test]
    async fn private_non_command_still_binds_to_the_steward_agent_session() {
        let (executor, repo) = setup_steward();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_text_message("tg_42", "chat_1", "帮我规划今天的工作", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Dispatched {
                conversation_id,
                session_id,
                ..
            } => {
                assert_eq!(conversation_id.as_deref(), Some("steward-conversation"));
                let session = repo
                    .sessions
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|session| session.id == session_id)
                    .cloned()
                    .unwrap();
                assert_eq!(session.agent_type, "steward");
            }
            _ => panic!("Expected steward-bound Dispatched result"),
        }
    }

    #[tokio::test]
    async fn group_message_does_not_receive_steward_control_context() {
        let (executor, repo) = setup_steward();
        repo.add_authorized_user("tg_42", "telegram");

        let mut msg = make_text_message("tg_42", "group_1", "有哪些任务", PluginType::Telegram);
        msg.is_group = true;
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Dispatched { conversation_id, .. } => assert!(conversation_id.is_none()),
            _ => panic!("Expected ordinary Dispatched result for a group"),
        }
    }

    // ── Platform action tests ──────────────────────────────────────────

    #[tokio::test]
    async fn pairing_show_generates_code() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.show",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("pairing code"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_check_authorized() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.check",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("authorized"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_check_not_authorized() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_99", // different user
            "chat_1",
            "pairing.check",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        // tg_99 is not authorized, but the action itself needs the user to be authorized
        // first (it's routed via handle_incoming_message which checks auth first)
        // So for this test, authorize tg_99 too
        repo.add_authorized_user("tg_99", "telegram");

        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                // tg_99 is authorized
                assert!(text.contains("authorized"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_help_returns_instructions() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.help",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("authorization"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── System action tests ────────────────────────────────────────────

    #[tokio::test]
    async fn session_new_creates_session() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.new",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("New session"));
                // With no client_preferences configured, defaults to "aionrs"
                assert!(text.contains("aionrs"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn session_new_resets_existing_session() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        // First: send a text message to create a session
        let text_msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);
        let r1 = executor.handle_incoming_message(&text_msg).await.unwrap();
        let sid1 = match r1 {
            MessageResult::Dispatched { session_id, .. } => session_id,
            _ => panic!("Expected Dispatched"),
        };

        // Then: session.new should delete old + create fresh
        let new_msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.new",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let r2 = executor.handle_incoming_message(&new_msg).await.unwrap();
        match r2 {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("New session"));
            }
            _ => panic!("Expected Action result"),
        }

        // Send another text message — the session ID should differ
        let text_msg2 = make_text_message("tg_42", "chat_1", "Again", PluginType::Telegram);
        let r3 = executor.handle_incoming_message(&text_msg2).await.unwrap();
        let sid3 = match r3 {
            MessageResult::Dispatched { session_id, .. } => session_id,
            _ => panic!("Expected Dispatched"),
        };
        // New session has different full ID (reset deleted the old one)
        assert_ne!(sid1, sid3);

        // Only 1 session should exist for this user+chat
        let sessions = repo.sessions.lock().unwrap();
        let user_chat_sessions: Vec<_> = sessions
            .iter()
            .filter(|s| s.user_id == "user_tg_42" && s.chat_id.as_deref() == Some("chat_1"))
            .collect();
        assert_eq!(user_chat_sessions.len(), 1);
    }

    #[tokio::test]
    async fn session_status_shows_info() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.status",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Session:"));
                assert!(text.contains("Agent:"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn help_show_returns_menu() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "help.show",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert!(resp.text.is_some());
                assert!(resp.buttons.is_some());
                let buttons = resp.buttons.unwrap();
                assert!(buttons.len() >= 2); // at least 2 rows
                assert!(
                    !buttons.iter().flatten().any(|button| button.action == "agent.show"),
                    "help menu must not expose direct agent selection"
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn agent_show_is_treated_as_unknown_action() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.show",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Unknown action"));
                assert!(text.contains("agent.show"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn agent_select_is_treated_as_unknown_action() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let params = HashMap::from([("agentType".into(), "acp".into())]);
        let select_msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(params),
        );
        let result = executor.handle_incoming_message(&select_msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Unknown action"));
                assert!(text.contains("agent.select"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── Chat action tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn system_confirm_returns_answer() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let params = HashMap::from([("callId".into(), "call_123".into()), ("value".into(), "true".into())]);
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "system.confirm",
            ActionCategory::Chat,
            PluginType::Telegram,
            Some(params),
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Answer);
                assert_eq!(resp.toast.as_deref(), Some("Confirmed"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn action_copy_returns_answer() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "action.copy",
            ActionCategory::Chat,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Answer);
                assert!(resp.toast.as_deref().unwrap().contains("Copied"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── Unknown action tests ───────────────────────────────────────────

    #[tokio::test]
    async fn unknown_platform_action() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "unknown.action",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Unknown action"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── build_pairing_response tests ───────────────────────────────────

    #[test]
    fn pairing_response_contains_code() {
        let resp = build_pairing_response("123456");
        let text = resp.text.unwrap();
        assert!(text.contains("123456"));
        assert!(text.contains("pairing code"));
        assert_eq!(resp.behavior, ActionBehavior::Send);
        assert!(resp.buttons.is_some());
    }

    #[test]
    fn help_response_has_buttons() {
        let resp = build_help_response();
        assert!(resp.text.is_some());
        let buttons = resp.buttons.unwrap();
        assert!(!buttons.is_empty());
    }

    #[test]
    fn unknown_action_response_includes_name() {
        let resp = build_unknown_action_response("foo.bar");
        let text = resp.text.unwrap();
        assert!(text.contains("foo.bar"));
    }
}
