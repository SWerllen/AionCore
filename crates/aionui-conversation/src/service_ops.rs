//! Agent-session operations on ConversationService.
//!
//! These forward to the active AgentInstance (via `self.task(id)`) for
//! config-options/usage/slash-commands/side-question queries, plus workspace
//! browsing that needs the conversations.extra.workspace field.
//!
//! Kept in a separate file from service.rs to avoid pushing that file
//! over 2000 lines.

use std::collections::HashMap;
use std::path::Component;

use aionui_ai_agent::{AcpError, AgentError};
use aionui_api_types::{
    AcpConfigOptionDto, AcpConfigSelectOptionDto, ConfigOptionConfirmation, GetConfigOptionsResponse,
    NativeGoalStateResponse, SetConfigOptionRequest, SetConfigOptionResponse, SetNativeGoalRequest,
    SideQuestionRequest, SideQuestionResponse, SlashCommandItem, WorkspaceBrowseQuery, WorkspaceEntry,
};
use aionui_common::{AgentKillReason, ErrorChain};
use aionui_db::SaveRuntimeStateParams;
use tracing::warn;

use crate::ConversationError;
use crate::service::{AssistantRuntimePreferenceUpdate, ConversationService};

const MAX_DIR_DEPTH: usize = 10;
const QODER_CONTEXT_WINDOW_OPTION_ID: &str = "context_window";
const QODER_CONTEXT_WINDOW_DEFAULT: &str = "default";

fn format_context_window(value: u64) -> String {
    if value >= 1_000_000 && value.is_multiple_of(1_000_000) {
        format!("{}M", value / 1_000_000)
    } else if value >= 1_000 && value.is_multiple_of(1_000) {
        format!("{}K", value / 1_000)
    } else {
        value.to_string()
    }
}

pub(crate) fn context_windows_for_model(available_models: &str, selected_model: Option<&str>) -> Vec<u64> {
    let Ok(catalog) = serde_json::from_str::<serde_json::Value>(available_models) else {
        return Vec::new();
    };
    let selected_model = selected_model.filter(|value| !value.trim().is_empty()).or_else(|| {
        catalog
            .get("current_model_id")
            .or_else(|| catalog.get("currentModelId"))
            .and_then(serde_json::Value::as_str)
    });
    let Some(selected_model) = selected_model else {
        return Vec::new();
    };
    let Some(models) = catalog
        .get("available_models")
        .or_else(|| catalog.get("availableModels"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    let Some(model) = models.iter().find(|model| {
        model
            .get("id")
            .or_else(|| model.get("modelId"))
            .and_then(serde_json::Value::as_str)
            == Some(selected_model)
    }) else {
        return Vec::new();
    };
    let mut windows = Vec::new();
    if let Some(values) = model
        .get("available_context_windows")
        .or_else(|| model.get("availableContextWindows"))
        .and_then(serde_json::Value::as_array)
    {
        for value in values
            .iter()
            .filter_map(serde_json::Value::as_u64)
            .filter(|value| *value > 0)
        {
            if !windows.contains(&value) {
                windows.push(value);
            }
        }
    }
    windows
}

impl ConversationService {
    // ── Config Options ──────────────────────────────────────────────

    /// Qoder supports `--context-window` in its native CLI, but its ACP
    /// `session/config_option` surface currently omits that setting. Project a
    /// small AionUi-owned option into the otherwise provider-owned snapshot;
    /// setting it persists the launch flag and recycles only this runtime.
    pub(crate) fn augment_qoder_context_window_options(
        extra: &str,
        context_windows: &[u64],
        mut config_options: Vec<AcpConfigOptionDto>,
    ) -> Vec<AcpConfigOptionDto> {
        let extra: serde_json::Value = serde_json::from_str(extra).unwrap_or(serde_json::Value::Null);
        if extra.get("backend").and_then(serde_json::Value::as_str) != Some("qoder") {
            return config_options;
        }
        let current_value = extra
            .get(QODER_CONTEXT_WINDOW_OPTION_ID)
            .and_then(serde_json::Value::as_u64)
            .filter(|value| context_windows.contains(value))
            .map(|value| value.to_string())
            .unwrap_or_else(|| QODER_CONTEXT_WINDOW_DEFAULT.to_owned());
        config_options.retain(|option| option.id != QODER_CONTEXT_WINDOW_OPTION_ID);
        let mut options = vec![AcpConfigSelectOptionDto {
            value: QODER_CONTEXT_WINDOW_DEFAULT.to_owned(),
            name: Some("Default".to_owned()),
            label: Some("Default".to_owned()),
            description: None,
        }];
        options.extend(context_windows.iter().copied().map(|value| {
            let label = format_context_window(value);
            AcpConfigSelectOptionDto {
                value: value.to_string(),
                name: Some(label.clone()),
                label: Some(label),
                description: None,
            }
        }));
        config_options.push(AcpConfigOptionDto {
            id: QODER_CONTEXT_WINDOW_OPTION_ID.to_owned(),
            name: None,
            label: None,
            description: None,
            category: Some(QODER_CONTEXT_WINDOW_OPTION_ID.to_owned()),
            option_type: "select".to_owned(),
            current_value: Some(current_value),
            options,
        });
        config_options
    }

    async fn qoder_context_windows(
        &self,
        user_id: &str,
        conversation_id: &str,
        extra: &str,
        selected_model: Option<&str>,
    ) -> Result<Vec<u64>, ConversationError> {
        let extra: serde_json::Value = serde_json::from_str(extra).unwrap_or(serde_json::Value::Null);
        if extra.get("backend").and_then(serde_json::Value::as_str) != Some("qoder") {
            return Ok(Vec::new());
        }
        let session = self.acp_session_repo().get_for_user(user_id, conversation_id).await?;
        let Some(session) = session else {
            return Ok(Vec::new());
        };
        let metadata = self
            .agent_metadata_repo()
            .get_for_user(user_id, &session.agent_id)
            .await?;
        let Some(available_models) = metadata.and_then(|row| row.available_models) else {
            return Ok(Vec::new());
        };
        let runtime_model = self
            .acp_session_repo()
            .load_runtime_state_for_user(user_id, conversation_id)
            .await?
            .and_then(|state| {
                state
                    .config_selections_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(raw).ok())
                    .and_then(|selections| selections.get("model").cloned())
                    .or(state.current_model_id)
            });
        let extra_model = extra
            .get("current_model_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(context_windows_for_model(
            &available_models,
            selected_model.or(runtime_model.as_deref()).or(extra_model.as_deref()),
        ))
    }

    pub(crate) async fn augment_qoder_context_window_options_for_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        extra: &str,
        config_options: Vec<AcpConfigOptionDto>,
    ) -> Result<Vec<AcpConfigOptionDto>, ConversationError> {
        let context_windows = self
            .qoder_context_windows(user_id, conversation_id, extra, None)
            .await?;
        Ok(Self::augment_qoder_context_window_options(
            extra,
            &context_windows,
            config_options,
        ))
    }

    async fn qoder_extra(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, ConversationError> {
        let row = self
            .conversation_repo()
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
        Ok((extra.get("backend").and_then(serde_json::Value::as_str) == Some("qoder")).then_some(row.extra))
    }

    async fn set_qoder_context_window(
        &self,
        user_id: &str,
        conversation_id: &str,
        value: &str,
    ) -> Result<SetConfigOptionResponse, ConversationError> {
        let extra = self
            .qoder_extra(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::BadRequest {
                reason: "context window is only supported by Qoder conversations".to_owned(),
            })?;
        let context_windows = self
            .qoder_context_windows(user_id, conversation_id, &extra, None)
            .await?;
        let context_window = if value == QODER_CONTEXT_WINDOW_DEFAULT {
            None
        } else {
            let value = value.parse::<u64>().map_err(|_| ConversationError::BadRequest {
                reason: "invalid Qoder context window".to_owned(),
            })?;
            if !context_windows.contains(&value) {
                return Err(ConversationError::BadRequest {
                    reason: format!("unsupported Qoder context window for the selected model: {value}"),
                });
            }
            Some(value)
        };
        if self.runtime_state().active_turn_id_for(conversation_id).is_some() {
            return Err(ConversationError::Busy {
                reason: "context window can only be changed while the conversation is idle".to_owned(),
            });
        }

        self.update_extra(
            user_id,
            conversation_id,
            serde_json::json!({ (QODER_CONTEXT_WINDOW_OPTION_ID): context_window }),
        )
        .await?;
        let restarted = self
            .restart_runtime(user_id, conversation_id, self.task_manager())
            .await?;
        Ok(SetConfigOptionResponse {
            confirmation: ConfigOptionConfirmation::Observed,
            config_options: Some(restarted.config_options),
        })
    }

    pub async fn get_config_options(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<GetConfigOptionsResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        if self.runtime_state().is_restarting(conversation_id) {
            return Err(ConversationError::RuntimeRestarting {
                conversation_id: conversation_id.to_owned(),
            });
        }
        let response = self
            .task(conversation_id)?
            .get_config_options()
            .await
            .map_err(ConversationError::from)?;
        let qoder_extra = self.qoder_extra(user_id, conversation_id).await?;
        Ok(GetConfigOptionsResponse {
            config_options: match qoder_extra {
                Some(extra) => {
                    let windows = self
                        .qoder_context_windows(user_id, conversation_id, &extra, None)
                        .await?;
                    Self::augment_qoder_context_window_options(&extra, &windows, response.config_options)
                }
                None => response.config_options,
            },
        })
    }

    /// Return the last model selection that was confirmed by the runtime.
    ///
    /// Generic config selections and the assistant snapshot are checked before
    /// the legacy ACP current-model field. Older Team builds could persist the
    /// first two after an observed switch while leaving that field stale.
    pub async fn confirmed_model_id(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<String>, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        let runtime_state = self
            .acp_session_repo()
            .load_runtime_state_for_user(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load runtime model state: {e}")))?;
        let config_selection = runtime_state
            .as_ref()
            .and_then(|state| state.config_selections_json.as_deref())
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(raw).ok())
            .and_then(|selections| selections.get("model").cloned())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if config_selection.is_some() {
            return Ok(config_selection);
        }

        let snapshot_model = self
            .conversation_repo()
            .get_assistant_snapshot(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load assistant model snapshot: {e}")))?
            .and_then(|snapshot| snapshot.resolved_model_id)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if snapshot_model.is_some() {
            return Ok(snapshot_model);
        }

        Ok(runtime_state
            .and_then(|state| state.current_model_id)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }

    /// Persist an already-observed model selection into every conversation
    /// source used when a Team member runtime is rebuilt.
    pub async fn persist_confirmed_model(
        &self,
        user_id: &str,
        conversation_id: &str,
        model: &str,
    ) -> Result<(), ConversationError> {
        let model = model.trim();
        if model.is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "model must not be empty".into(),
            });
        }
        self.update_extra(
            user_id,
            conversation_id,
            serde_json::json!({ "current_model_id": model }),
        )
        .await?;

        let runtime_state = self
            .acp_session_repo()
            .load_runtime_state_for_user(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load runtime model state: {e}")))?;
        let mut selections = runtime_state
            .and_then(|state| state.config_selections_json)
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
            .unwrap_or_default();
        selections.insert("model".to_owned(), model.to_owned());
        let selections_json = serde_json::to_string(&selections)
            .map_err(|e| ConversationError::internal(format!("Failed to serialize runtime model selection: {e}")))?;
        self.acp_session_repo()
            .save_runtime_state_for_user(
                user_id,
                conversation_id,
                &SaveRuntimeStateParams {
                    current_model_id: Some(Some(model)),
                    config_selections_json: Some(Some(&selections_json)),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to persist runtime model: {e}")))?;
        self.persist_runtime_assistant_snapshot(
            user_id,
            conversation_id,
            AssistantRuntimePreferenceUpdate {
                model: Some(model),
                permission: None,
                thought_level: None,
            },
        )
        .await?;
        Ok(())
    }

    pub async fn set_config_option(
        &self,
        user_id: &str,
        conversation_id: &str,
        option_id: &str,
        req: SetConfigOptionRequest,
    ) -> Result<SetConfigOptionResponse, ConversationError> {
        if option_id.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "option_id must not be empty".into(),
            });
        }
        if req.value.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "value must not be empty".into(),
            });
        }
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        if self.runtime_state().is_restarting(conversation_id) {
            return Err(ConversationError::RuntimeRestarting {
                conversation_id: conversation_id.to_owned(),
            });
        }
        if option_id == QODER_CONTEXT_WINDOW_OPTION_ID && self.qoder_extra(user_id, conversation_id).await?.is_some() {
            return self
                .set_qoder_context_window(user_id, conversation_id, req.value.trim())
                .await;
        }
        let agent = self.task(conversation_id)?;
        let mut response = match agent.set_config_option(option_id, &req.value).await {
            Ok(response) => response,
            Err(err @ AgentError::Acp(AcpError::NotConnected)) => {
                warn!(
                    conversation_id,
                    option_id,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    error = %ErrorChain(&err),
                    "ACP config option failed because protocol is disconnected; evicting task"
                );
                self.task_manager()
                    .kill_and_wait(conversation_id, Some(AgentKillReason::AgentErrorRecovery))
                    .await;
                return Err(ConversationError::from(err));
            }
            Err(err) => return Err(ConversationError::from(err)),
        };

        // Mirror runtime model/mode/thought-level switches into the persisted assistant
        // snapshot + preference so the next conversation seeded from this
        // assistant in `auto` mode reflects the latest pick.
        //
        // `PendingNextTurn` counts: it means the agent WILL apply the value from the next
        // turn, so it is the user's settled choice, merely not in force yet. codex reports
        // every mode switch that way (its schema documents `permissions` as "for
        // subsequent turns"), so excluding it would strip preference memory from every
        // codex conversation. `CommandAck` still does NOT count — it means nothing could
        // be established either way. Persistence failures are logged but do not roll back
        // the user-facing config switch.
        if matches!(
            response.confirmation,
            ConfigOptionConfirmation::Observed | ConfigOptionConfirmation::PendingNextTurn
        ) {
            let category = response
                .config_options
                .as_ref()
                .and_then(|options| options.iter().find(|option| option.id == option_id))
                .and_then(|option| option.category.as_deref())
                .unwrap_or(option_id);
            let updates = match category {
                "model" => Some(AssistantRuntimePreferenceUpdate {
                    model: Some(req.value.as_str()),
                    permission: None,
                    thought_level: None,
                }),
                "mode" => Some(AssistantRuntimePreferenceUpdate {
                    model: None,
                    permission: Some(req.value.as_str()),
                    thought_level: None,
                }),
                "thought_level" | "reasoning_effort" => Some(AssistantRuntimePreferenceUpdate {
                    model: None,
                    permission: None,
                    thought_level: Some(req.value.as_str()),
                }),
                _ => None,
            };
            if let Some(updates) = updates {
                if let Err(err) = self
                    .persist_runtime_assistant_snapshot(user_id, conversation_id, updates)
                    .await
                {
                    warn!(
                        conversation_id,
                        option_id,
                        error = %ErrorChain(&err),
                        "Failed to persist runtime assistant snapshot after set_config_option",
                    );
                }
                if let Err(err) = self
                    .persist_runtime_assistant_preferences(user_id, conversation_id, updates)
                    .await
                {
                    warn!(
                        conversation_id,
                        option_id,
                        error = %ErrorChain(&err),
                        "Failed to persist runtime assistant preferences after set_config_option",
                    );
                }
            }
        }

        if let Some(extra) = self.qoder_extra(user_id, conversation_id).await?
            && let Some(config_options) = response.config_options.take()
        {
            let selected_model = config_options
                .iter()
                .find(|option| option.id == option_id)
                .and_then(|option| (option.id == "model").then_some(req.value.as_str()));
            let context_windows = self
                .qoder_context_windows(user_id, conversation_id, &extra, selected_model)
                .await?;
            response.config_options = Some(Self::augment_qoder_context_window_options(
                &extra,
                &context_windows,
                config_options,
            ));
        }

        Ok(response)
    }

    // ── Usage / Slash commands ──────────────────────────────────────

    pub async fn get_usage(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<serde_json::Value>, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        // A reaped task must NOT mean "no usage". The indicator's whole point is
        // to survive switching away and back, and the snapshot it needs is
        // already durable in `acp_session.session_config.runtime.context_usage`
        // — `SessionAgentTask::get_usage` reads it from there too. Requiring a
        // live task here made the figure vanish exactly when the user returned
        // to an idle conversation.
        if let Ok(task) = self.task(conversation_id) {
            return task.get_usage().await.map_err(ConversationError::from);
        }
        let state = self
            .acp_session_repo()
            .load_runtime_state_for_user(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load usage state: {e}")))?;
        Ok(state
            .and_then(|s| s.context_usage_json)
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok()))
    }

    pub async fn get_slash_commands(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<SlashCommandItem>, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        self.task(conversation_id)?
            .get_slash_commands()
            .await
            .map_err(ConversationError::from)
    }

    // ── Provider-native goals ──────────────────────────────────────

    pub async fn get_native_goal(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<NativeGoalStateResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        let state = self
            .task(conversation_id)?
            .get_native_goal()
            .await
            .map_err(ConversationError::from)?;
        tracing::info!(
            conversation_id,
            provider = %state.provider,
            supported = state.capabilities.supported,
            control_mode = ?state.capabilities.control_mode,
            goal_present = state.goal.is_some(),
            "provider_native_goal_read"
        );
        Ok(state)
    }

    pub async fn set_native_goal(
        &self,
        user_id: &str,
        conversation_id: &str,
        request: SetNativeGoalRequest,
    ) -> Result<NativeGoalStateResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        if request.clear_token_budget && request.token_budget.is_some() {
            return Err(ConversationError::BadRequest {
                reason: "token_budget and clear_token_budget cannot be used together".into(),
            });
        }
        if let Some(objective) = request.objective.as_deref()
            && objective.trim().is_empty()
        {
            return Err(ConversationError::BadRequest {
                reason: "objective must not be empty".into(),
            });
        }
        if request.objective.is_none()
            && request.status.is_none()
            && request.token_budget.is_none()
            && !request.clear_token_budget
        {
            return Err(ConversationError::BadRequest {
                reason: "native goal update requires objective, status, or token_budget".into(),
            });
        }
        let state = self
            .task(conversation_id)?
            .set_native_goal(&request)
            .await
            .map_err(ConversationError::from)?;
        tracing::info!(
            conversation_id,
            provider = %state.provider,
            status = ?state.goal.as_ref().map(|goal| goal.status),
            "provider_native_goal_set"
        );
        Ok(state)
    }

    pub async fn clear_native_goal(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<NativeGoalStateResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        let state = self
            .task(conversation_id)?
            .clear_native_goal()
            .await
            .map_err(ConversationError::from)?;
        tracing::info!(
            conversation_id,
            provider = %state.provider,
            cleared = state.cleared.unwrap_or(false),
            "provider_native_goal_cleared"
        );
        Ok(state)
    }

    // ── Side question ───────────────────────────────────────────────

    pub async fn handle_side_question(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SideQuestionRequest,
    ) -> Result<SideQuestionResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        // `AgentInstance::handle_side_question` already validates that the
        // question is non-empty; no need to duplicate the check here.
        self.task(conversation_id)?
            .handle_side_question(req)
            .await
            .map_err(ConversationError::from)
    }

    async fn ensure_owned_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), ConversationError> {
        let exists = self
            .conversation_repo()
            .get(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load conversation: {e}")))?
            .is_some();
        if exists {
            Ok(())
        } else {
            Err(ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })
        }
    }

    // ── Workspace browsing ──────────────────────────────────────────

    /// Enumerate entries under `query.path` inside the conversation's
    /// workspace root. Enforces workspace isolation (no traversal outside
    /// the root, with an allowance for symlinked sub-directories) and a
    /// depth cap of [`MAX_DIR_DEPTH`].
    pub async fn browse_workspace(
        &self,
        user_id: &str,
        conversation_id: &str,
        query: WorkspaceBrowseQuery,
    ) -> Result<Vec<WorkspaceEntry>, ConversationError> {
        if query.path.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "path must not be empty".into(),
            });
        }

        let row = self
            .conversation_repo()
            .get(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load conversation: {e}")))?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let extra: serde_json::Value = serde_json::from_str(&row.extra)
            .map_err(|e| ConversationError::internal(format!("Invalid extra JSON: {e}")))?;
        let workspace = extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if workspace.is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "Conversation has no workspace assigned".into(),
            });
        }

        let relative_path = query.path.trim_start_matches('/');
        let relative_path_obj = std::path::Path::new(relative_path);
        if relative_path_obj
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(ConversationError::BadRequest {
                reason: "Path traversal outside workspace is not allowed".into(),
            });
        }

        // Resolve the browsed path relative to the workspace root
        let base = std::path::Path::new(&workspace);
        let browse_path = if relative_path.is_empty() {
            base.to_path_buf()
        } else {
            base.join(relative_path_obj)
        };

        // Security: reject direct traversal outside the workspace root, but allow
        // symlinked directories mounted inside the workspace (e.g. native skill
        // dirs that point at the builtin skills corpus under data-dir).
        let canonical_base = base
            .canonicalize()
            .map_err(|e| ConversationError::internal(format!("Failed to resolve workspace path: {e}")))?;
        let canonical_browse = browse_path
            .canonicalize()
            .map_err(|_| ConversationError::not_found_reason("Directory not found"))?;
        if !browse_path.starts_with(base) && !canonical_browse.starts_with(&canonical_base) {
            return Err(ConversationError::BadRequest {
                reason: "Path traversal outside workspace is not allowed".into(),
            });
        }

        // Check depth limit
        let depth = relative_path_obj.components().count();
        if depth > MAX_DIR_DEPTH {
            return Err(ConversationError::BadRequest {
                reason: format!("Directory depth exceeds maximum of {MAX_DIR_DEPTH}"),
            });
        }

        let mut entries = Vec::new();
        let mut dir_reader = tokio::fs::read_dir(&canonical_browse)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to read directory: {e}")))?;

        while let Ok(Some(entry)) = dir_reader.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();

            // Apply search filter if provided
            if let Some(ref search) = query.search
                && !search.is_empty()
                && !name.to_lowercase().contains(&search.to_lowercase())
            {
                continue;
            }

            let entry_path = entry.path();
            let metadata = tokio::fs::metadata(&entry_path)
                .await
                .map_err(|e| ConversationError::internal(format!("Failed to read entry metadata: {e}")))?;

            let entry_type = if metadata.is_dir() { "directory" } else { "file" };

            entries.push(WorkspaceEntry {
                name,
                entry_type: entry_type.into(),
            });
        }

        // Sort: directories first, then alphabetically
        entries.sort_by(|a, b| {
            let type_cmp = a.entry_type.cmp(&b.entry_type);
            if type_cmp == std::cmp::Ordering::Equal {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            } else {
                type_cmp
            }
        });

        Ok(entries)
    }
}
