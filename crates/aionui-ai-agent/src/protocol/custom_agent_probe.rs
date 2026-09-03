//! Two-step probe for custom ACP agents.
//!
//! Step 1: `which`/`where` — resolve the first token of `command` on
//!         `$PATH`. Bounded by `execFileSync`-equivalent 5 s timeout.
//! Step 2: Spawn the CLI via `CliAgentProcess::spawn_for_sdk`, connect
//!         an `AcpProtocol` (which owns the ACP `initialize` handshake
//!         with a built-in 30 s timeout), then shut down cleanly.
//!
//! The same function is called by:
//!   - `POST /api/agents/custom/try-connect`  (manual "test connection" button)
//!   - `AgentService::create/update_custom_agent`   (test-on-save)
//!
//! Both paths produce identical outcomes / error text.

use std::collections::HashMap;
use std::time::Duration;

use aionui_api_types::{AgentHandshake, TryConnectCustomAgentResponse};
use aionui_common::{CommandSpec, EnvVar};

use crate::protocol::acp_init_budget::{INIT_TIMEOUT_SECS, InitBudget};
use aionui_runtime::{NodeRuntimeProgressReporter, ResolvedCommand, ensure_runtime_command_with_reporter};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use crate::capability::cli_process::CliAgentProcess;
use crate::protocol::acp::AcpProtocol;
use crate::protocol::error::AcpError;

use agent_client_protocol::schema::v1::{
    NewSessionRequest, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOptions,
    SessionNotification, SessionUpdate,
};

/// Step 2 overall timeout. Belt-and-suspenders: `AcpProtocol::connect`
/// already caps the initialize RPC at 30 s, but a CLI that hangs
/// before writing any ACP frame at all is covered by this outer cap.
const STEP2_TIMEOUT: Duration = Duration::from_secs(35);

/// Qoder advertises reasoning effort dynamically: the option set changes after
/// each `session/set_model`. A normal reachability probe stops at session/new,
/// while the catalog probe used by Agent management needs enough time to walk
/// the model catalog once and persist the per-model matrix.
const MODEL_REASONING_STEP2_TIMEOUT: Duration = Duration::from_secs(120);
const MODEL_CONFIG_UPDATE_TIMEOUT: Duration = Duration::from_secs(2);
// A cold Qoder launch can spend more than 20 seconds loading its local account
// and model catalog (observed by the real CLI health-check path).  This probe is
// the source of truth for context-window choices, and its successful result is
// persisted, so give the one-time refresh the same realistic budget as the ACP
// catalog walk instead of silently dropping the selector on a cold start.
const QODER_NATIVE_CATALOG_TIMEOUT: Duration = Duration::from_secs(60);

/// Grace period for the child to exit on its own after stdin close, before
/// we fall back to SIGKILL on the whole process group. Keep this short because
/// manual connection tests should return promptly after the ACP probe finishes.
const PROBE_KILL_GRACE: Duration = Duration::from_millis(500);

/// Probe a custom ACP agent.
///
/// Returns `Success` only if both `which` and the ACP `initialize`
/// handshake succeed. Any failure short-circuits into the
/// corresponding variant.
pub async fn try_connect_custom_agent(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> TryConnectCustomAgentResponse {
    try_connect_custom_agent_with_catalog(command, args, env, reporter)
        .await
        .0
}

/// As [`try_connect_custom_agent`], plus the catalog the probe's `session/new`
/// advertised. The probe already pays for a full spawn + `initialize` +
/// `session/new`; callers that persist agent metadata use this variant so that
/// data is stored instead of thrown away. The partial is `Some` only on a
/// successful session whose agent advertised at least one of modes / models /
/// config options — an auth-gated or failing probe yields `None`, never an
/// empty write that would blank an existing catalog.
pub async fn try_connect_custom_agent_with_catalog(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> (TryConnectCustomAgentResponse, Option<Box<AgentHandshake>>) {
    try_connect_custom_agent_with_catalog_internal(command, args, env, reporter, false).await
}

/// As [`try_connect_custom_agent_with_catalog`], additionally walking every
/// advertised model and recording the reasoning-effort values the Agent emits
/// after that model is selected. This is intentionally opt-in: most ACP agents
/// expose a static catalog and should not pay for N model-switch RPCs during a
/// health check. Qoder uses this path because its reasoning range is model
/// dependent and is only discoverable after `session/set_model`.
pub async fn try_connect_custom_agent_with_model_reasoning_catalog(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> (TryConnectCustomAgentResponse, Option<Box<AgentHandshake>>) {
    try_connect_custom_agent_with_catalog_internal(command, args, env, reporter, true).await
}

async fn try_connect_custom_agent_with_catalog_internal(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
    probe_model_reasoning: bool,
) -> (TryConnectCustomAgentResponse, Option<Box<AgentHandshake>>) {
    // ── Step 1 — which check ────────────────────────────────────────
    let head = first_token(command);
    let resolved = match ensure_runtime_command_with_reporter(head, reporter).await {
        Ok(resolved) => resolved,
        Err(error) => {
            return (
                TryConnectCustomAgentResponse::FailCli {
                    error: error.to_string(),
                },
                None,
            );
        }
    };
    debug!(program = %resolved.program.display(), "probe step 1 ok");
    let native_catalog_command = resolved.clone();

    // ── Step 2 — spawn + ACP initialize ─────────────────────────────
    let proc = match spawn_probe_process(resolved, args, env).await {
        Ok(proc) => proc,
        Err(msg) => return (TryConnectCustomAgentResponse::FailAcp { error: msg }, None),
    };

    let step2_timeout = if probe_model_reasoning {
        MODEL_REASONING_STEP2_TIMEOUT
    } else {
        STEP2_TIMEOUT
    };
    let outcome = match tokio::time::timeout(step2_timeout, run_handshake(&proc, probe_model_reasoning)).await {
        Ok(outcome) => {
            let catalog = match &outcome {
                ProbeOutcome::Ok(catalog) => catalog.clone(),
                _ => None,
            };
            (outcome.into_response(), catalog)
        }
        Err(_) => (
            TryConnectCustomAgentResponse::FailAcp {
                error: format!("ACP handshake did not complete within {}s", step2_timeout.as_secs()),
            },
            None,
        ),
    };

    // Always tear down the whole process group. `kill_on_drop(true)` only
    // signals the direct child (e.g. `npm exec ...`) — wrapper CLIs spawn
    // grandchildren (`openclaw-acp`) that survive unless we SIGKILL the
    // group explicitly via `proc.kill()`.
    if let Err(error) = proc.kill(PROBE_KILL_GRACE).await {
        warn!(pid = proc.pid(), error = %error, "probe failed to kill process group");
    }

    let (response, mut catalog) = outcome;
    if probe_model_reasoning && matches!(&response, TryConnectCustomAgentResponse::Success) {
        match probe_qoder_native_model_catalog(&native_catalog_command, args, env).await {
            Ok(model_contexts) => {
                if let Some(handshake) = catalog.as_deref_mut() {
                    attach_model_context_windows(handshake, &model_contexts);
                }
                debug!(
                    model_count = model_contexts.len(),
                    "Qoder native model context catalog loaded"
                );
            }
            Err(error) => {
                // The native catalog enriches an otherwise valid ACP handshake.
                // Keep the Agent online when this optional projection fails; the
                // UI will offer only the provider default instead of guessed sizes.
                warn!(error = %error, "Qoder native model context catalog unavailable");
            }
        }
    }

    (response, catalog)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QoderModelContextConfig {
    windows: Vec<u64>,
    default_window: Option<u64>,
}

async fn probe_qoder_native_model_catalog(
    resolved: &ResolvedCommand,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<HashMap<String, QoderModelContextConfig>, String> {
    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().filter(|arg| arg.as_str() != "--acp").cloned());
    final_args.extend([
        "-p".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--no-session-persistence".to_owned(),
        "--strict-mcp-config".to_owned(),
        "--mcp-config".to_owned(),
        r#"{"mcpServers":{}}"#.to_owned(),
        "--setting-sources".to_owned(),
        String::new(),
    ]);

    let mut final_env: Vec<EnvVar> = env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    final_env.extend(resolved.env.iter().map(|(name, value)| EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    let spec = CommandSpec {
        command: resolved.program.clone(),
        args: final_args,
        env: final_env,
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
    };
    let proc = CliAgentProcess::spawn_for_sdk(spec)
        .await
        .map_err(|error| format!("spawn failed: {error}"))?;
    let result = match tokio::time::timeout(QODER_NATIVE_CATALOG_TIMEOUT, async {
        let Some((mut stdin, stdout)) = proc.take_stdio().await else {
            return Err("stdio not available after native Qoder spawn".to_owned());
        };
        let request = serde_json::json!({
            "type": "control_request",
            "request_id": "aionui-model-catalog",
            "request": {
                "type": "initialize",
                "supportsCatalogReadyInitialize": true,
                "supportsAvailableModelsUpdate": true,
            },
        });
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|error| format!("write initialize request failed: {error}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|error| format!("close initialize request failed: {error}"))?;

        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|error| format!("read catalog response failed: {error}"))?
        {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("type").and_then(serde_json::Value::as_str) != Some("control_response")
                || value
                    .pointer("/response/request_id")
                    .and_then(serde_json::Value::as_str)
                    != Some("aionui-model-catalog")
            {
                continue;
            }
            if value.pointer("/response/subtype").and_then(serde_json::Value::as_str) != Some("success") {
                let error = value
                    .pointer("/response/error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Qoder native catalog request failed");
                return Err(error.to_owned());
            }
            return parse_qoder_native_model_catalog(&value);
        }
        Err("Qoder native catalog response ended before initialize completed".to_owned())
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Qoder native catalog did not complete within {}s",
            QODER_NATIVE_CATALOG_TIMEOUT.as_secs()
        )),
    };

    if let Err(error) = proc.kill(PROBE_KILL_GRACE).await {
        warn!(pid = proc.pid(), error = %error, "native Qoder catalog probe failed to kill process group");
    }
    result
}

fn parse_qoder_native_model_catalog(
    value: &serde_json::Value,
) -> Result<HashMap<String, QoderModelContextConfig>, String> {
    let models = value
        .pointer("/response/response/models")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Qoder native catalog response did not contain models".to_owned())?;
    let mut result = HashMap::new();
    for model in models {
        let Some(model_id) = model
            .get("modelId")
            .or_else(|| model.get("value"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let mut windows = Vec::new();
        if let Some(values) = model
            .get("availableContextWindows")
            .or_else(|| model.get("available_context_windows"))
            .and_then(serde_json::Value::as_array)
        {
            for window in values
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .filter(|value| *value > 0)
            {
                if !windows.contains(&window) {
                    windows.push(window);
                }
            }
        }
        if windows.is_empty() {
            continue;
        }
        let default_window = model
            .get("defaultContextWindow")
            .or_else(|| model.get("default_context_window"))
            .and_then(serde_json::Value::as_u64)
            .filter(|value| windows.contains(value));
        result.insert(
            model_id.to_owned(),
            QoderModelContextConfig {
                windows,
                default_window,
            },
        );
    }
    if result.is_empty() {
        return Err("Qoder native catalog contained no context-window metadata".to_owned());
    }
    Ok(result)
}

fn attach_model_context_windows(handshake: &mut AgentHandshake, catalog: &HashMap<String, QoderModelContextConfig>) {
    let Some(models) = handshake
        .available_models
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|payload| payload.get_mut("available_models"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    for model in models {
        let Some(model_id) = model.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(context) = catalog.get(model_id) else {
            continue;
        };
        if let Some(model) = model.as_object_mut() {
            model.insert(
                "available_context_windows".to_owned(),
                serde_json::json!(context.windows),
            );
            if let Some(default_window) = context.default_window {
                model.insert("default_context_window".to_owned(), serde_json::json!(default_window));
            }
        }
    }
}

fn first_token(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

async fn spawn_probe_process(
    resolved: ResolvedCommand,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<CliAgentProcess, String> {
    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().cloned());

    let mut final_env: Vec<EnvVar> = env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    final_env.extend(resolved.env.iter().map(|(name, value)| EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    let spec = CommandSpec {
        command: resolved.program,
        args: final_args,
        env: final_env,
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
    };

    CliAgentProcess::spawn_for_sdk(spec)
        .await
        .map_err(|e| format!("spawn failed: {e}"))
}

/// Result of the Step 2 probe (`initialize` + `session/new`).
///
/// The probe reaches `session/new` so it can tell "reachable but not
/// authorized" (`Auth`) apart from other ACP failures (`Fail`) — `initialize`
/// alone returns `authMethods` even for already-authorized agents and cannot
/// make this distinction.
enum ProbeOutcome {
    /// Carries the catalog the successful `session/new` advertised (modes /
    /// models / config options), already projected into the same shape the live
    /// session path persists. `None` when the agent advertised nothing. Boxed to
    /// keep the enum small: the success payload dwarfs the error strings.
    Ok(Option<Box<AgentHandshake>>),
    Auth(String),
    Fail(String),
}

impl ProbeOutcome {
    fn into_response(self) -> TryConnectCustomAgentResponse {
        match self {
            ProbeOutcome::Ok(_) => TryConnectCustomAgentResponse::Success,
            ProbeOutcome::Auth(error) => TryConnectCustomAgentResponse::FailAuth { error },
            ProbeOutcome::Fail(error) => TryConnectCustomAgentResponse::FailAcp { error },
        }
    }
}

async fn run_handshake(proc: &CliAgentProcess, probe_model_reasoning: bool) -> ProbeOutcome {
    let Some((stdin, stdout)) = proc.take_stdio().await else {
        return ProbeOutcome::Fail("stdio not available after spawn_for_sdk".to_string());
    };

    // Throwaway channels — a probe session never sends a prompt, so no events,
    // permission requests, or notifications are consumed.
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (permission_tx, _permission_rx) = mpsc::channel(4);
    let (notification_tx, mut notification_rx) = mpsc::channel(32);

    // Race the ACP initialize handshake against the child process exiting.
    // A misconfigured CLI (e.g. an invalid package launcher command) exits
    // almost immediately with a non-zero status; without this race the
    // `AcpProtocol::connect` call would block on its whole initialize budget
    // waiting for an `initialize` reply that will never arrive.
    //
    // The probe passes the steady-state budget rather than deriving a
    // cold-start one: the caller already caps this handshake at
    // `STEP2_TIMEOUT`, so a longer inner budget could never elapse. Keeping
    // the inner budget strictly below that cap also preserves which timeout
    // reports the failure — the inner one, as `InitTimeout`.
    let connect = AcpProtocol::connect(
        stdin,
        stdout,
        event_tx,
        permission_tx,
        notification_tx,
        "custom-agent-probe",
        None,
        InitBudget {
            timeout: Duration::from_secs(INIT_TIMEOUT_SECS),
            cold_start: false,
        },
    );
    let protocol = tokio::select! {
        biased;
        res = connect => match res {
            Ok(protocol) => protocol,
            Err(e) => return ProbeOutcome::Fail(format!("ACP initialize failed: {e}")),
        },
        exit = proc.wait_for_exit() => {
            let stderr = proc.take_stderr().await;
            let stderr = stderr.trim();
            let status = match exit {
                Some(s) => format!("{s}"),
                None => "unknown".to_string(),
            };
            return if stderr.is_empty() {
                ProbeOutcome::Fail(format!("CLI exited before ACP initialize completed (status={status})"))
            } else {
                ProbeOutcome::Fail(format!("CLI exited before ACP initialize completed (status={status}): {stderr}"))
            };
        }
    };

    // `initialize` only proves the agent speaks ACP, not that it is usable.
    // Open a real session (no prompt) so an auth-gated agent surfaces its
    // `auth_required` error here instead of silently appearing "online".
    // Keep the response: it carries the agent's advertised modes / models /
    // config options, which the caller persists into `agent_metadata` so the
    // picker is populated before the user ever opens a conversation. Discarding
    // it meant a probed-online agent still showed an empty picker.
    let outcome = match protocol.new_session(NewSessionRequest::new(std::env::temp_dir())).await {
        Ok((response, legacy_models)) => {
            // Same extraction the live session path performs (`agent_session_flow`):
            // models ride beside the response because the SDK dropped the field.
            let models = legacy_models
                .as_ref()
                .and_then(crate::manager::acp::legacy_session_model::LegacySessionModelState::from_state_value);
            let reasoning_catalog = if probe_model_reasoning {
                let session_id = response.session_id.to_string();
                probe_model_reasoning_efforts(&protocol, &mut notification_rx, &session_id, models.as_ref()).await
            } else {
                HashMap::new()
            };
            let mut partial = crate::manager::acp::catalog_forwarder::catalog_partial_from_session_new(
                response.modes.as_ref(),
                models.as_ref(),
                response.config_options.as_deref(),
            );
            if let Some(partial) = partial.as_mut()
                && !reasoning_catalog.is_empty()
            {
                attach_model_reasoning_efforts(partial, &reasoning_catalog);
            }
            ProbeOutcome::Ok(partial.map(Box::new))
        }
        Err(AcpError::AuthRequired) => {
            ProbeOutcome::Auth("Agent reachable but requires login/authorization".to_string())
        }
        Err(e) => ProbeOutcome::Fail(format!("ACP session/new failed: {e}")),
    };

    // Drop `protocol` so its shutdown oneshot fires before the outer cleanup
    // path (or the drop guard for timeout-cancelled callers) reaps the process
    // tree. The probe session is throwaway; the process-group kill in the
    // caller tears down the session along with the CLI.
    drop(protocol);
    outcome
}

async fn probe_model_reasoning_efforts(
    protocol: &AcpProtocol,
    notification_rx: &mut mpsc::Receiver<SessionNotification>,
    session_id: &str,
    models: Option<&crate::manager::acp::legacy_session_model::LegacySessionModelState>,
) -> HashMap<String, Vec<String>> {
    let mut catalog = HashMap::new();
    let Some(models) = models else {
        return catalog;
    };

    // Probe the session's current model last. Some agents (notably Qoder) do
    // not emit a config update when `set_model` repeats the current value, so
    // probing it first would leave that model's capability unknown. Switching
    // away and back guarantees an update for the current/default model too.
    for model_id in model_ids_in_probe_order(models) {
        if let Err(error) = protocol.set_model(session_id, &model_id).await {
            warn!(model_id, error = %error, "model reasoning probe: set_model failed");
            continue;
        }

        let deadline = tokio::time::Instant::now() + MODEL_CONFIG_UPDATE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let notification = match tokio::time::timeout(remaining, notification_rx.recv()).await {
                Ok(Some(notification)) => notification,
                _ => break,
            };
            let SessionUpdate::ConfigOptionUpdate(update) = notification.update else {
                continue;
            };
            if current_model_from_config_options(&update.config_options).as_deref() != Some(model_id.as_str()) {
                continue;
            }
            catalog.insert(
                model_id.clone(),
                reasoning_efforts_from_config_options(&update.config_options),
            );
            break;
        }
    }

    catalog
}

fn model_ids_in_probe_order(
    models: &crate::manager::acp::legacy_session_model::LegacySessionModelState,
) -> Vec<String> {
    models
        .available_models
        .iter()
        .filter(|model| model.model_id != models.current_model_id)
        .chain(
            models
                .available_models
                .iter()
                .filter(|model| model.model_id == models.current_model_id),
        )
        .map(|model| model.model_id.clone())
        .collect()
}

fn select_option(option: &SessionConfigOption) -> Option<&agent_client_protocol::schema::v1::SessionConfigSelect> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(select),
        _ => None,
    }
}

fn flattened_select_values(select: &agent_client_protocol::schema::v1::SessionConfigSelect) -> Vec<String> {
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().map(|option| option.value.to_string()).collect()
        }
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(|option| option.value.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

fn current_model_from_config_options(options: &[SessionConfigOption]) -> Option<String> {
    options
        .iter()
        .find(|option| {
            option.category.as_ref() == Some(&SessionConfigOptionCategory::Model)
                || matches!(option.id.to_string().as_str(), "model" | "models")
        })
        .and_then(select_option)
        .map(|select| select.current_value.to_string())
}

fn reasoning_efforts_from_config_options(options: &[SessionConfigOption]) -> Vec<String> {
    options
        .iter()
        .find(|option| {
            option.category.as_ref() == Some(&SessionConfigOptionCategory::ThoughtLevel)
                || matches!(
                    option.id.to_string().as_str(),
                    "effort" | "reasoning_effort" | "thought_level"
                )
        })
        .and_then(select_option)
        .map(flattened_select_values)
        .unwrap_or_default()
}

fn attach_model_reasoning_efforts(handshake: &mut AgentHandshake, catalog: &HashMap<String, Vec<String>>) {
    let Some(models) = handshake
        .available_models
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|payload| payload.get_mut("available_models"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    for model in models {
        let Some(model_id) = model.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(efforts) = catalog.get(model_id) else {
            continue;
        };
        if let Some(model) = model.as_object_mut() {
            // Preserve an explicit empty array. It means "probed and this model
            // has no reasoning control", which is different from an old
            // unprobed catalog entry where the field is absent.
            model.insert("reasoning_efforts".to_owned(), serde_json::json!(efforts));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{SessionConfigSelectGroup, SessionConfigSelectOption};
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn parses_qoder_native_context_windows_per_model() {
        let response = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "aionui-model-catalog",
                "response": {
                    "models": [
                        {
                            "modelId": "ultimate",
                            "availableContextWindows": [200000, 400000, 1000000, 400000],
                            "defaultContextWindow": 200000
                        },
                        {
                            "modelId": "kmodel",
                            "availableContextWindows": [256000],
                            "defaultContextWindow": 256000
                        },
                        {
                            "modelId": "missing-metadata"
                        }
                    ]
                }
            }
        });

        let catalog = parse_qoder_native_model_catalog(&response).expect("native catalog");

        assert_eq!(catalog["ultimate"].windows, vec![200_000, 400_000, 1_000_000]);
        assert_eq!(catalog["ultimate"].default_window, Some(200_000));
        assert_eq!(catalog["kmodel"].windows, vec![256_000]);
        assert!(!catalog.contains_key("missing-metadata"));
    }

    #[test]
    fn attaches_qoder_context_windows_without_inventing_missing_models() {
        let mut handshake = AgentHandshake {
            available_models: Some(json!({
                "available_models": [
                    {"id": "ultimate", "label": "Ultimate"},
                    {"id": "unknown", "label": "Unknown"}
                ]
            })),
            ..Default::default()
        };
        let catalog = HashMap::from([(
            "ultimate".to_owned(),
            QoderModelContextConfig {
                windows: vec![200_000, 400_000, 1_000_000],
                default_window: Some(200_000),
            },
        )]);

        attach_model_context_windows(&mut handshake, &catalog);

        let models = &handshake.available_models.expect("models")["available_models"];
        assert_eq!(
            models[0]["available_context_windows"],
            json!([200_000, 400_000, 1_000_000])
        );
        assert_eq!(models[0]["default_context_window"], json!(200_000));
        assert!(models[1].get("available_context_windows").is_none());
    }

    #[tokio::test]
    async fn probe_returns_fail_cli_when_command_missing() {
        let resp = try_connect_custom_agent("aionui-definitely-does-not-exist-xyz", &[], &HashMap::new(), None).await;
        match resp {
            TryConnectCustomAgentResponse::FailCli { error } => {
                let lower = error.to_lowercase();
                assert!(
                    lower.contains("not found") || lower.contains("no such") || lower.contains("was not found"),
                    "expected 'not found' style message, got: {error}"
                );
            }
            other => panic!("expected FailCli, got {other:?}"),
        }
    }

    #[test]
    fn extracts_reasoning_efforts_from_the_current_model_config() {
        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "qmodel_38max",
                vec![SessionConfigSelectOption::new("qmodel_38max", "Qwen3.8-Max")],
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "reasoning_effort",
                "Reasoning",
                "medium",
                vec![
                    SessionConfigSelectOption::new("xhigh", "Extra High"),
                    SessionConfigSelectOption::new("medium", "Medium"),
                    SessionConfigSelectOption::new("low", "Low"),
                    SessionConfigSelectOption::new("none", "None"),
                ],
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];

        assert_eq!(
            current_model_from_config_options(&options).as_deref(),
            Some("qmodel_38max")
        );
        assert_eq!(
            reasoning_efforts_from_config_options(&options),
            vec!["xhigh", "medium", "low", "none"]
        );
    }

    #[test]
    fn extracts_grouped_reasoning_efforts_and_preserves_known_empty_models() {
        let grouped = vec![
            SessionConfigOption::select(
                "reasoning_effort",
                "Reasoning",
                "high",
                vec![SessionConfigSelectGroup::new(
                    "levels",
                    "Levels",
                    vec![
                        SessionConfigSelectOption::new("max", "Max"),
                        SessionConfigSelectOption::new("high", "High"),
                    ],
                )],
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];
        assert_eq!(reasoning_efforts_from_config_options(&grouped), vec!["max", "high"]);

        let mut handshake = AgentHandshake {
            available_models: Some(json!({
                "available_models": [
                    {"id": "gmodel", "label": "GLM-5.3"},
                    {"id": "lite", "label": "Lite"}
                ]
            })),
            ..Default::default()
        };
        attach_model_reasoning_efforts(
            &mut handshake,
            &HashMap::from([
                ("gmodel".to_owned(), vec!["max".to_owned(), "high".to_owned()]),
                ("lite".to_owned(), Vec::new()),
            ]),
        );

        let models = &handshake.available_models.unwrap()["available_models"];
        assert_eq!(models[0]["reasoning_efforts"], json!(["max", "high"]));
        assert_eq!(models[1]["reasoning_efforts"], json!([]));
    }

    #[test]
    fn probes_the_current_model_last() {
        let models = crate::manager::acp::legacy_session_model::LegacySessionModelState::new(
            "auto",
            vec![
                crate::manager::acp::legacy_session_model::LegacyModelEntry::new("auto", "Auto"),
                crate::manager::acp::legacy_session_model::LegacyModelEntry::new("fast", "Fast"),
                crate::manager::acp::legacy_session_model::LegacyModelEntry::new("deep", "Deep"),
            ],
        );

        assert_eq!(model_ids_in_probe_order(&models), vec!["fast", "deep", "auto"]);
    }

    #[tokio::test]
    async fn probe_returns_fail_acp_when_command_is_noop() {
        // `true` exits 0 immediately — Step 1 passes (on PATH), but the
        // process dies before ACP initialize completes, so Step 2 maps
        // to FailAcp.
        if cfg!(windows) {
            // `true` is a cmd builtin on Windows, not a standalone exe.
            return;
        }
        let resp = try_connect_custom_agent("true", &[], &HashMap::new(), None).await;
        assert!(
            matches!(resp, TryConnectCustomAgentResponse::FailAcp { .. }),
            "expected FailAcp, got {resp:?}"
        );
    }

    /// Regression for the production leak: a probe that talks to a wrapper
    /// CLI (`npm exec ...`, etc.) historically left the wrapper's grandchild
    /// process alive when the probe returned, because cleanup relied on
    /// `kill_on_drop(true)` which only signals the direct child. Repeated
    /// connection tests could otherwise accumulate zombie `openclaw-acp`
    /// processes.
    ///
    /// We exercise the public entry point with a CLI that exits immediately
    /// after backgrounding a long-lived grandchild — that's the production
    /// shape `npm exec openclaw --acp` collapses into when its own ACP
    /// handshake fails. The probe will see the wrapper exit (ACP fail), but
    /// by that point the grandchild has been forked. The fix must SIGKILL
    /// the whole process group before returning, so the grandchild dies too.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_kills_grandchild_left_behind_by_wrapper() {
        use std::time::Duration;
        use tokio::time::Instant;

        fn is_pid_alive(pid: i32) -> bool {
            unsafe { libc::kill(pid, 0) == 0 }
        }

        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_owned();
        // Background a grandchild, write its pid, then exit. The probe will
        // observe `proc.wait_for_exit()` race the ACP handshake and return
        // FailAcp — the grandchild keeps running unless cleanup kills it.
        let script = format!("sleep 600 & printf '%s' \"$!\" > '{}'", marker_path.display());

        let resp = try_connect_custom_agent("sh", &["-c".to_string(), script], &HashMap::new(), None).await;
        assert!(
            matches!(resp, TryConnectCustomAgentResponse::FailAcp { .. }),
            "wrapper exits before ACP handshake; expected FailAcp, got {resp:?}"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut marker_contents = String::new();
        while Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(&marker_path)
                && !s.trim().is_empty()
            {
                marker_contents = s;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let grandchild_pid: i32 = marker_contents.trim().parse().unwrap_or_else(|_| {
            panic!("wrapper did not write the grandchild pid: {marker_contents:?}");
        });

        // Give the OS a brief moment to reap after the probe returned.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_pid_alive(grandchild_pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Cleanup so a failing test does not leave an actual leak.
        unsafe {
            libc::kill(grandchild_pid, libc::SIGKILL);
        }
        panic!("grandchild pid={grandchild_pid} survived the probe — process group cleanup is broken");
    }
}
