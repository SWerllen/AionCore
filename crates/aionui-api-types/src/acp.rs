use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Request body for detecting an ACP CLI executable.
///
/// `backend` is a vendor label (e.g. "claude"). The service resolves it
/// against the `agent_metadata` catalog.
#[derive(Debug, Deserialize)]
pub struct DetectCliRequest {
    pub backend: String,
}

/// Response for CLI detection.
#[derive(Debug, Serialize)]
pub struct DetectCliResponse {
    /// Path to the detected CLI, `None` if not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Response for ACP environment variables.
#[derive(Debug, Serialize)]
pub struct AcpEnvResponse {
    pub env: HashMap<String, String>,
}

/// Response for agent session mode.
#[derive(Debug, Serialize)]
pub struct AgentModeResponse {
    pub mode: String,
    pub initialized: bool,
}

/// Request body for setting session mode.
#[derive(Debug, Deserialize)]
pub struct SetModeRequest {
    pub mode: String,
}

/// Request body for setting ACP session model.
#[derive(Debug, Deserialize)]
pub struct SetModelRequest {
    pub model_id: String,
}

/// A single available model entry in the frontend-facing model info response.
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfoEntry {
    pub id: String,
    pub label: String,
}

/// Frontend-compatible model info response.
///
/// Maps from the SDK's camelCase `SessionModelState` to the snake_case
/// `AcpModelInfo` format the renderer expects.
#[derive(Debug, Serialize)]
pub struct GetModelInfoResponse {
    pub model_info: Option<ModelInfoPayload>,
}

/// A single select option inside an ACP config option.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpConfigSelectOptionDto {
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Frontend-facing ACP config option. Always serializes with snake_case field names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpConfigOptionDto {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(rename = "type")]
    pub option_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_value: Option<String>,
    #[serde(default)]
    pub options: Vec<AcpConfigSelectOptionDto>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOptionConfirmation {
    /// The agent applied it: the next tool-approval decision already uses the new value.
    Observed,
    /// Accepted, but the agent applies it only from the NEXT turn — the in-flight turn
    /// keeps the old value. The frontend must show the option as pending rather than
    /// switched, and must NOT treat this as a failure.
    ///
    /// Exists because reporting `Observed` here was self-fulfilling: the task caches the
    /// requested value as an optimistic override and reads it straight back, so the user
    /// was told a switch had landed while the agent still enforced the old mode.
    PendingNextTurn,
    /// The command was accepted but no confirmation could be established either way.
    CommandAck,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SetConfigOptionRequest {
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetConfigOptionsResponse {
    pub config_options: Vec<AcpConfigOptionDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetConfigOptionResponse {
    pub confirmation: ConfigOptionConfirmation,
    pub config_options: Option<Vec<AcpConfigOptionDto>>,
}

/// How AionUi may interact with a provider-owned persistent goal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeGoalControlMode {
    Structured,
    SlashCommand,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeGoalBudgetKind {
    Tokens,
    None,
}

/// Provider goal lifecycle. The two `*Limited` aliases preserve the Codex
/// app-server wire spelling while AionUi exposes snake_case JSON.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeGoalStatus {
    Active,
    Paused,
    Blocked,
    #[serde(alias = "usageLimited")]
    UsageLimited,
    #[serde(alias = "budgetLimited")]
    BudgetLimited,
    Complete,
}

impl NativeGoalStatus {
    pub fn provider_value(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usageLimited",
            Self::BudgetLimited => "budgetLimited",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeGoalCapabilities {
    pub supported: bool,
    #[serde(alias = "controlMode")]
    pub control_mode: NativeGoalControlMode,
    #[serde(alias = "canSet")]
    pub can_set: bool,
    #[serde(alias = "canGet")]
    pub can_get: bool,
    #[serde(alias = "canClear")]
    pub can_clear: bool,
    #[serde(alias = "canPause")]
    pub can_pause: bool,
    #[serde(alias = "canResume")]
    pub can_resume: bool,
    #[serde(alias = "budgetKind")]
    pub budget_kind: NativeGoalBudgetKind,
}

impl NativeGoalCapabilities {
    pub fn structured_tokens() -> Self {
        Self {
            supported: true,
            control_mode: NativeGoalControlMode::Structured,
            can_set: true,
            can_get: true,
            can_clear: true,
            can_pause: true,
            can_resume: true,
            budget_kind: NativeGoalBudgetKind::Tokens,
        }
    }

    pub fn slash_command() -> Self {
        Self {
            supported: true,
            control_mode: NativeGoalControlMode::SlashCommand,
            can_set: false,
            can_get: false,
            can_clear: false,
            can_pause: false,
            can_resume: false,
            budget_kind: NativeGoalBudgetKind::None,
        }
    }

    pub fn unsupported() -> Self {
        Self {
            supported: false,
            ..Self::slash_command()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeGoalSnapshot {
    #[serde(alias = "threadId")]
    pub thread_id: String,
    pub objective: String,
    pub status: NativeGoalStatus,
    #[serde(alias = "tokenBudget")]
    pub token_budget: Option<u64>,
    #[serde(alias = "tokensUsed")]
    pub tokens_used: u64,
    #[serde(alias = "timeUsedSeconds")]
    pub time_used_seconds: u64,
    #[serde(alias = "createdAt")]
    pub created_at: i64,
    #[serde(alias = "updatedAt")]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeGoalStateResponse {
    pub provider: String,
    pub capabilities: NativeGoalCapabilities,
    pub goal: Option<NativeGoalSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared: Option<bool>,
}

impl NativeGoalStateResponse {
    pub fn slash_command(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            capabilities: NativeGoalCapabilities::slash_command(),
            goal: None,
            cleared: None,
        }
    }

    pub fn unsupported(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            capabilities: NativeGoalCapabilities::unsupported(),
            goal: None,
            cleared: None,
        }
    }
}

/// Partial update for a provider-owned goal. `clear_token_budget` is explicit
/// because JSON `null` and an omitted `Option` otherwise deserialize alike.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SetNativeGoalRequest {
    pub objective: Option<String>,
    pub status: Option<NativeGoalStatus>,
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub clear_token_budget: bool,
}

/// Inner model info payload matching the frontend's `AcpModelInfo` type.
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfoPayload {
    pub current_model_id: Option<String>,
    pub current_model_label: Option<String>,
    pub available_models: Vec<ModelInfoEntry>,
}

/// Request body for probing model information.
#[derive(Debug, Deserialize)]
pub struct ProbeModelRequest {
    pub backend: String,
}

/// Request body for probing a custom ACP agent.
///
/// Two-step check: Step 1 resolves `command` on `$PATH`; Step 2 spawns
/// the CLI and performs an ACP `initialize` handshake. The same
/// function is called from the dedicated endpoint (manual test button)
/// and from the create/update path (test-on-save).
#[derive(Debug, Clone, Deserialize)]
pub struct TryConnectCustomAgentRequest {
    pub command: String,
    #[serde(default)]
    pub acp_args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub runtime_scope_id: Option<String>,
}

/// Outcome of [`TryConnectCustomAgentRequest`].
///
/// Tagged enum: `step` distinguishes the states the frontend's Alert component
/// renders (success → green, fail_cli → red, fail_acp → yellow, fail_auth →
/// yellow with a "needs login" hint). `error` carries a human-readable reason
/// for the failure variants.
///
/// The probe reaches `session/new` (not just `initialize`), so `fail_auth`
/// distinguishes "reachable but not authorized" (ACP `auth_required`,
/// JSON-RPC `-32000`) from other ACP failures — `initialize` alone cannot
/// tell these apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum TryConnectCustomAgentResponse {
    Success,
    FailCli { error: String },
    FailAcp { error: String },
    FailAuth { error: String },
}

/// Query parameters for workspace browse.
#[derive(Debug, Deserialize)]
pub struct WorkspaceBrowseQuery {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

/// A file or directory entry in the workspace browse response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
}

/// Request body for side question.
#[derive(Debug, Deserialize)]
pub struct SideQuestionRequest {
    pub question: String,
}

/// Response for side question.
#[derive(Debug, Serialize)]
pub struct SideQuestionResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detect_cli_request_serde() {
        let json = json!({ "backend": "claude" });
        let req: DetectCliRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.backend, "claude");
    }

    #[test]
    fn detect_cli_response_with_path() {
        let resp = DetectCliResponse {
            path: Some("/usr/local/bin/claude".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["path"], "/usr/local/bin/claude");
    }

    #[test]
    fn detect_cli_response_without_path() {
        let resp = DetectCliResponse { path: None };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("path").is_none());
    }

    #[test]
    fn set_mode_request_serde() {
        let json = json!({ "mode": "code" });
        let req: SetModeRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.mode, "code");
    }

    #[test]
    fn set_model_request_serde() {
        let json = json!({ "model_id": "claude-sonnet-4" });
        let req: SetModelRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.model_id, "claude-sonnet-4");
    }

    #[test]
    fn config_options_response_serializes_snake_case() {
        let resp = GetConfigOptionsResponse {
            config_options: vec![AcpConfigOptionDto {
                id: "reasoning_effort".to_owned(),
                name: Some("Reasoning Effort".to_owned()),
                label: None,
                description: None,
                category: Some("thought_level".to_owned()),
                option_type: "select".to_owned(),
                current_value: Some("high".to_owned()),
                options: vec![AcpConfigSelectOptionDto {
                    value: "high".to_owned(),
                    name: Some("High".to_owned()),
                    label: None,
                    description: None,
                }],
            }],
        };

        let value = serde_json::to_value(resp).unwrap();
        assert_eq!(value["config_options"][0]["current_value"], "high");
        assert_eq!(value["config_options"][0]["type"], "select");
        assert!(value["config_options"][0].get("currentValue").is_none());
    }

    #[test]
    fn set_config_option_response_serializes_command_ack_without_snapshot() {
        let resp = SetConfigOptionResponse {
            confirmation: ConfigOptionConfirmation::CommandAck,
            config_options: None,
        };

        let value = serde_json::to_value(resp).unwrap();
        assert_eq!(value["confirmation"], "command_ack");
        assert!(value["config_options"].is_null());
    }

    #[test]
    fn native_goal_deserializes_codex_camel_case_without_leaking_it_to_http() {
        let parsed: NativeGoalStateResponse = serde_json::from_value(json!({
            "provider": "codex",
            "capabilities": {
                "supported": true,
                "controlMode": "structured",
                "canSet": true,
                "canGet": true,
                "canClear": true,
                "canPause": true,
                "canResume": true,
                "budgetKind": "tokens"
            },
            "goal": {
                "threadId": "thread-1",
                "objective": "Ship it",
                "status": "usageLimited",
                "tokenBudget": 12000,
                "tokensUsed": 300,
                "timeUsedSeconds": 12,
                "createdAt": 1,
                "updatedAt": 2
            }
        }))
        .unwrap();
        assert_eq!(parsed.goal.as_ref().unwrap().status, NativeGoalStatus::UsageLimited);
        let http = serde_json::to_value(parsed).unwrap();
        assert_eq!(http["capabilities"]["control_mode"], "structured");
        assert_eq!(http["goal"]["status"], "usage_limited");
        assert_eq!(http["goal"]["token_budget"], 12000);
    }

    #[test]
    fn try_connect_custom_agent_request_serde() {
        let json = json!({
            "command": "/path/to/agent",
            "acp_args": ["--flag"],
            "env": { "KEY": "value" },
            "runtime_scope_id": "custom-agent:test"
        });
        let req: TryConnectCustomAgentRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.command, "/path/to/agent");
        assert_eq!(req.acp_args, vec!["--flag"]);
        assert_eq!(req.env.get("KEY"), Some(&"value".into()));
        assert_eq!(req.runtime_scope_id.as_deref(), Some("custom-agent:test"));
    }

    #[test]
    fn try_connect_custom_agent_request_defaults() {
        let json = json!({ "command": "/bin/test" });
        let req: TryConnectCustomAgentRequest = serde_json::from_value(json).unwrap();
        assert!(req.acp_args.is_empty());
        assert!(req.env.is_empty());
        assert!(req.runtime_scope_id.is_none());
    }

    #[test]
    fn try_connect_response_tag_serializes() {
        use super::TryConnectCustomAgentResponse;
        let ok = TryConnectCustomAgentResponse::Success;
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            serde_json::json!({"step":"success"})
        );

        let fail = TryConnectCustomAgentResponse::FailCli {
            error: "not found".into(),
        };
        assert_eq!(
            serde_json::to_value(&fail).unwrap(),
            serde_json::json!({"step":"fail_cli","error":"not found"})
        );

        // Reachable-but-unauthorized is its own tag so the UI can show a
        // "needs login" hint instead of a generic ACP failure.
        let auth = TryConnectCustomAgentResponse::FailAuth {
            error: "requires login".into(),
        };
        assert_eq!(
            serde_json::to_value(&auth).unwrap(),
            serde_json::json!({"step":"fail_auth","error":"requires login"})
        );
    }

    #[test]
    fn env_response_serde() {
        let resp = AcpEnvResponse {
            env: HashMap::from([("PATH".into(), "/usr/bin".into()), ("HOME".into(), "/home/user".into())]),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["env"]["PATH"], "/usr/bin");
    }

    #[test]
    fn probe_model_request_serde() {
        let json = json!({ "backend": "claude" });
        let req: ProbeModelRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.backend, "claude");
    }

    /// The confirmation wire strings are a cross-repo contract: AionUi's
    /// `AcpConfigOptionConfirmation` matches on these literals, and its picker branches on
    /// them to decide between "switched", "pending" and "failed". A rename here that looked
    /// harmless in Rust would silently push the frontend into its error path, so pin the
    /// exact bytes rather than trusting the derive.
    #[test]
    fn config_option_confirmation_wire_strings_are_stable() {
        for (variant, expected) in [
            (ConfigOptionConfirmation::Observed, "observed"),
            (ConfigOptionConfirmation::PendingNextTurn, "pending_next_turn"),
            (ConfigOptionConfirmation::CommandAck, "command_ack"),
        ] {
            assert_eq!(serde_json::to_value(variant).unwrap(), json!(expected));
            let round_tripped: ConfigOptionConfirmation = serde_json::from_value(json!(expected)).unwrap();
            assert_eq!(round_tripped, variant);
        }
    }
}
