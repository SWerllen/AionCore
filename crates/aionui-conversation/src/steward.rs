use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
    time::Duration,
};

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    AskStewardTaskRequest, AssistantConversationRequest, BindStewardTaskSessionRequest, BootstrapStewardRequest,
    ConversationResponse, ConversationRuntimeStateKind, ConversationRuntimeSummary, CreateConversationRequest,
    CreateStewardTaskRequest, DEFAULT_STEWARD_ASSISTANT_ID, DispatchStewardTaskRequest, ListConversationsQuery,
    ListMessagesQuery, MessageResponse, ResolveStewardTaskRequest, ResumeStewardTaskRequest, STEWARD_MCP_SERVER_NAME,
    SearchMessagesQuery, SendMessageRequest, SendMessageResponse, SessionMcpServer, SessionMcpTransport,
    SlashCommandItem, StewardCommandResponse, StewardExecutionState, StewardOverviewResponse, StewardProfileResponse,
    StewardSessionRole, StewardTaskCandidateResponse, StewardTaskEventResponse, StewardTaskInquiryResponse,
    StewardTaskLifecycle, StewardTaskResponse, StewardTaskSessionResponse, StewardUnregisteredConversationResponse,
    SwitchStewardAssistantRequest, UpdateConversationRequest, UpdateStewardTaskRequest,
};
use aionui_common::{
    ConversationSource, ConversationStatus, MessagePosition, MessageStatus, MessageType, generate_short_id, now_ms,
};
use aionui_db::models::MessageRow;
use aionui_db::{
    IStewardRepository, StewardProfileRow, StewardReportOutboxRow, StewardTaskEventRow, StewardTaskFilters,
    StewardTaskRow, StewardTaskSessionRow,
};

use crate::{ConversationError, ConversationService};

const STEWARD_PROMPT: &str = r#"You are the user's personal task steward in AionUi. Your durable source of truth is the aionui-steward MCP, not this chat history. Before creating work, search for an existing task or related session. Keep task lifecycle and execution state separate. The steward overview contains both registered durable tasks and unregistered top-level conversations; when the user asks what tasks exist, report both groups and never say there are none while unregistered_conversations is non-empty. Reuse or resume a suitable session when safe; create a new session and workspace only when needed. Delegate implementation to the task's primary session, whose own native harness may work solo or activate its embedded expert team. Read and summarize status, blockers, interruptions, and next actions. When the user asks the primary leader a question or asks what it is doing, use steward_ask_task and include the returned leader reply in your answer; never stop after saying the question was sent. A successful dispatch means the existing primary is being started; the server watches for real execution, performs at most one safe recovery of the same persisted message, and reports a timeout if it still does not start. Never create or bind a replacement primary after a dispatch timeout unless the user explicitly asks for a new session. Only archive or restore a conversation after the user explicitly requests that exact target. Resolve names through the overview first and ask when multiple conversations match. Never archive this steward conversation or an individual team worker; archiving a team leader archives the whole team. Never perform destructive, publishing, payment, or external-message actions without explicit user authorization. Update the durable task after material progress or a blocker."#;

const TASK_LEADER_PROMPT: &str = r#"This conversation is the primary execution session for a durable AionUi steward task. Work through your native harness and native MCP/skills. You may remain a single agent or activate the embedded expert team when complexity justifies it. Keep the steward informed through durable task state and clear final summaries; do not treat process exit as task completion."#;

const DISPATCH_START_PROBE_TIMEOUT: Duration = Duration::from_secs(7);
const DISPATCH_START_RECOVERY_TIMEOUT: Duration = Duration::from_secs(7);
const DISPATCH_START_POLL_INTERVAL: Duration = Duration::from_millis(500);
const LEADER_REPLY_TIMEOUT: Duration = Duration::from_secs(180);
const LEADER_REPLY_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StewardArchiveTarget {
    Conversation(String),
    Team(String),
}

#[async_trait::async_trait]
pub trait StewardControlPort: Send + Sync {
    async fn set_archived(&self, user_id: &str, target: StewardArchiveTarget, archived: bool) -> Result<(), String>;
}

/// Optional delivery boundary for proactive steward reports. The conversation
/// inbox is written by [`StewardService`] itself; the composition layer uses
/// this port to forward the same durable report to the most recently bound IM
/// chat without coupling the conversation crate to channel plugins.
#[async_trait::async_trait]
pub trait StewardReportDeliveryPort: Send + Sync {
    async fn deliver_im_report(
        &self,
        user_id: &str,
        steward_conversation_id: &str,
        content: &str,
    ) -> Result<usize, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedStewardCommand {
    Help,
    Tasks,
    Status(Option<String>),
    Workers(Option<String>),
    Resume(Option<String>),
    Archive(String),
    Restore(String),
    Ask { target: Option<String>, question: String },
}

impl ParsedStewardCommand {
    fn name(&self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Tasks => "tasks",
            Self::Status(_) => "status",
            Self::Workers(_) => "workers",
            Self::Resume(_) => "resume",
            Self::Archive(_) => "archive",
            Self::Restore(_) => "restore",
            Self::Ask { .. } => "ask",
        }
    }
}

#[derive(Clone)]
pub struct StewardService {
    repo: Arc<dyn IStewardRepository>,
    conversation: ConversationService,
    task_manager: Arc<dyn IWorkerTaskManager>,
    helper_binary: String,
    bootstrap_lock: Arc<tokio::sync::Mutex<()>>,
    control_port: Arc<OnceLock<Arc<dyn StewardControlPort>>>,
    report_delivery_port: Arc<OnceLock<Arc<dyn StewardReportDeliveryPort>>>,
}

impl StewardService {
    pub fn new(
        repo: Arc<dyn IStewardRepository>,
        conversation: ConversationService,
        task_manager: Arc<dyn IWorkerTaskManager>,
        helper_binary: String,
    ) -> Self {
        Self {
            repo,
            conversation,
            task_manager,
            helper_binary,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            control_port: Arc::new(OnceLock::new()),
            report_delivery_port: Arc::new(OnceLock::new()),
        }
    }

    pub fn set_control_port(&self, port: Arc<dyn StewardControlPort>) {
        let _ = self.control_port.set(port);
    }

    pub fn set_report_delivery_port(&self, port: Arc<dyn StewardReportDeliveryPort>) {
        let _ = self.report_delivery_port.set(port);
    }

    /// Consumes normalized terminal runtime events. Only runs/turns returned by
    /// a durable steward dispatch are eligible, so unrelated conversations and
    /// worker child turns cannot generate reports.
    pub async fn handle_terminal_event(
        &self,
        event_name: &str,
        data: &serde_json::Value,
    ) -> Result<bool, ConversationError> {
        let run_id_field = match event_name {
            "team.runCompleted" | "team.runFailed" | "team.runCancelled" => "team_run_id",
            "turn.completed" => "turn_id",
            _ => return Ok(false),
        };
        let Some(user_id) = data.get("user_id").and_then(serde_json::Value::as_str) else {
            return Ok(false);
        };
        let Some(run_id) = data.get(run_id_field).and_then(serde_json::Value::as_str) else {
            return Ok(false);
        };
        let Some(mut task) = self.repo.find_task_by_dispatch_run(user_id, run_id).await? else {
            return Ok(false);
        };
        let mut terminal_event = event_name;

        // Embedded-team leaders may also emit a per-turn terminal frame. The
        // team terminal event is authoritative because workers can still be
        // queued after the leader turn finishes.
        if event_name == "turn.completed" {
            let primary = self.primary_conversation(&task.id).await?;
            let conversation = self.conversation.get(user_id, &primary).await?;
            if conversation
                .extra
                .get("embedded_team_id")
                .and_then(serde_json::Value::as_str)
                .is_some()
            {
                return Ok(false);
            }
            let messages = self
                .conversation
                .list_messages(
                    user_id,
                    &primary,
                    ListMessagesQuery {
                        limit: Some(10),
                        before: None,
                        after: None,
                        anchor_message_id: None,
                        content_mode: None,
                    },
                )
                .await?;
            if messages
                .items
                .iter()
                .rev()
                .find(|message| message.position == Some(MessagePosition::Left))
                .is_some_and(|message| message.status == Some(MessageStatus::Error))
            {
                terminal_event = "turn.failed";
            }
        }

        let Some(steward_conversation_id) = self
            .repo
            .get_profile(user_id)
            .await?
            .and_then(|profile| profile.conversation_id)
        else {
            tracing::warn!(user_id, task_id = %task.id, run_id, "steward report skipped because profile has no conversation");
            return Ok(false);
        };

        let terminal_label = match terminal_event {
            "team.runFailed" | "turn.failed" => "失败",
            "team.runCancelled" => "已取消",
            _ => "已完成",
        };
        let run_result = if terminal_event == "team.runCompleted" || terminal_event == "turn.completed" {
            self.latest_run_result(user_id, &task.id, run_id).await?
        } else {
            None
        };
        let progress = run_result
            .as_deref()
            .or(task.progress_summary.as_deref())
            .unwrap_or("执行会话已结束，请打开原会话查看结果。");
        let progress_label = if run_result.is_some() {
            "Leader 结果"
        } else {
            "进展"
        };
        let mut content = format!(
            "任务「{}」本轮执行{}。\n{}：{}",
            task.title, terminal_label, progress_label, progress
        );
        if let Some(next_action) = task.next_action.as_deref() {
            content.push_str("\n下一步：");
            content.push_str(next_action);
        }
        let blockers: Vec<String> = serde_json::from_str(&task.blockers).unwrap_or_default();
        if !blockers.is_empty() {
            content.push_str("\n阻塞：");
            content.push_str(&blockers.join("；"));
        }

        let now = now_ms();
        if matches!(terminal_event, "team.runFailed" | "turn.failed") {
            task.execution_state = "failed".to_owned();
        } else if event_name == "team.runCancelled" {
            task.execution_state = "interrupted".to_owned();
        } else if task.execution_state == "running" {
            task.execution_state = "idle".to_owned();
        }
        task.updated_at = now;
        self.repo.update_task(&task).await?;

        let report = StewardReportOutboxRow {
            id: generate_short_id(),
            user_id: user_id.to_owned(),
            task_id: task.id.clone(),
            steward_conversation_id,
            run_id: run_id.to_owned(),
            terminal_event: terminal_event.to_owned(),
            content,
            inbox_delivered_at: None,
            im_delivered_at: None,
            attempts: 0,
            next_attempt_at: now,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let inserted = self.repo.enqueue_report(&report).await?;
        if inserted {
            self.append_event(
                &task.id,
                "runtime",
                "completion_report_queued",
                serde_json::json!({"run_id": run_id, "terminal_event": terminal_event, "report_id": report.id}),
            )
            .await?;
        }
        self.deliver_pending_reports().await?;
        Ok(inserted)
    }

    /// Drains persisted reports. It is safe to call after every event and from
    /// a periodic startup worker; each destination has its own checkpoint.
    pub async fn deliver_pending_reports(&self) -> Result<usize, ConversationError> {
        let reports = self.repo.list_pending_reports(now_ms(), 50).await?;
        let mut completed = 0;
        for report in reports {
            if report.inbox_delivered_at.is_none() {
                let message_id = format!("steward-report-{}", report.id);
                let row = MessageRow {
                    id: message_id.clone(),
                    conversation_id: report.steward_conversation_id.clone(),
                    msg_id: Some(message_id),
                    r#type: "text".to_owned(),
                    content: serde_json::json!({
                        "content": report.content,
                        "_meta": {
                            "kind": "steward_report",
                            "steward_report": true,
                            "task_id": report.task_id,
                            "run_id": report.run_id,
                            "terminal_event": report.terminal_event,
                        }
                    })
                    .to_string(),
                    position: Some("left".to_owned()),
                    status: Some("finish".to_owned()),
                    hidden: false,
                    created_at: report.created_at,
                    backend_turn_id: Some(format!("steward-report:{}", report.run_id)),
                };
                if let Err(error) = self.conversation.upsert_raw_message(&report.user_id, &row).await {
                    self.defer_report(&report, &error.to_string()).await?;
                    continue;
                }
                self.repo.mark_report_inbox_delivered(&report.id, now_ms()).await?;
            }

            if report.im_delivered_at.is_none() {
                let Some(port) = self.report_delivery_port.get() else {
                    self.defer_report(&report, "IM report delivery port is not ready")
                        .await?;
                    continue;
                };
                match port
                    .deliver_im_report(&report.user_id, &report.steward_conversation_id, &report.content)
                    .await
                {
                    Ok(target_count) => {
                        self.repo.mark_report_im_delivered(&report.id, now_ms()).await?;
                        self.append_event(
                            &report.task_id,
                            "runtime",
                            "completion_report_delivered",
                            serde_json::json!({
                                "run_id": report.run_id,
                                "report_id": report.id,
                                "im_target_count": target_count,
                            }),
                        )
                        .await?;
                        completed += 1;
                    }
                    Err(error) => {
                        self.defer_report(&report, &error).await?;
                    }
                }
            }
        }
        Ok(completed)
    }

    async fn defer_report(&self, report: &StewardReportOutboxRow, error: &str) -> Result<(), ConversationError> {
        let attempts = report.attempts.saturating_add(1);
        let exponent = attempts.clamp(0, 6) as u32;
        let delay_ms = 5_000_i64.saturating_mul(1_i64 << exponent).min(300_000);
        let now = now_ms();
        self.repo
            .record_report_failure(&report.id, attempts, now.saturating_add(delay_ms), error, now)
            .await?;
        tracing::warn!(
            report_id = %report.id,
            task_id = %report.task_id,
            attempts,
            error,
            "steward completion report delivery deferred"
        );
        Ok(())
    }

    /// Returns the primary leader's final text produced after the durable
    /// dispatch for this exact run. The dispatch timestamp prevents a
    /// completed run with no visible output from re-reporting an older answer.
    async fn latest_run_result(
        &self,
        user_id: &str,
        task_id: &str,
        run_id: &str,
    ) -> Result<Option<String>, ConversationError> {
        let dispatched_at = self
            .repo
            .list_events(task_id, 200)
            .await?
            .into_iter()
            .find(|event| {
                event.event_type == "task_dispatched"
                    && serde_json::from_str::<serde_json::Value>(&event.payload)
                        .ok()
                        .and_then(|payload| {
                            payload
                                .get("turn_id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        })
                        .as_deref()
                        == Some(run_id)
            })
            .map(|event| event.created_at);
        let Some(dispatched_at) = dispatched_at else {
            return Ok(None);
        };

        let primary = self.primary_conversation(task_id).await?;
        let messages = self
            .conversation
            .list_messages(
                user_id,
                &primary,
                ListMessagesQuery {
                    limit: Some(100),
                    before: None,
                    after: None,
                    anchor_message_id: None,
                    content_mode: None,
                },
            )
            .await?;

        Ok(messages.items.iter().rev().find_map(|message| {
            if message.created_at < dispatched_at
                || message.hidden
                || message.position != Some(MessagePosition::Left)
                || message.r#type != MessageType::Text
                || message.status != Some(MessageStatus::Finish)
            {
                return None;
            }
            message
                .content
                .get("content")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|content| !content.is_empty())
                .map(str::to_owned)
        }))
    }

    pub async fn bootstrap(
        &self,
        user_id: &str,
        request: BootstrapStewardRequest,
    ) -> Result<StewardProfileResponse, ConversationError> {
        // Conversation creation and profile upsert live in separate domains.
        // Serialize the short bootstrap section so concurrent browser loads do
        // not leave an unreferenced duplicate steward conversation behind.
        let _bootstrap_guard = self.bootstrap_lock.lock().await;
        if let Some(profile) = self.repo.get_profile(user_id).await?
            && let Some(conversation_id) = profile.conversation_id
            && request
                .assistant_id
                .as_deref()
                .is_none_or(|requested| profile.assistant_id.as_deref() == Some(requested))
            && let Ok(mut conversation) = self.conversation.get(user_id, &conversation_id).await
        {
            self.ensure_steward_control_snapshot(user_id, &conversation).await?;
            if conversation.name_source.as_deref() != Some("user") {
                conversation = self.pin_steward_name(user_id, &conversation.id).await?;
            }
            return Ok(profile_response(
                &conversation,
                profile
                    .assistant_id
                    .unwrap_or_else(|| DEFAULT_STEWARD_ASSISTANT_ID.to_owned()),
            ));
        }

        let assistant_id = request
            .assistant_id
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_STEWARD_ASSISTANT_ID.to_owned());
        let mut extra = serde_json::json!({
            "steward": true,
            "preset_context": STEWARD_PROMPT,
            "selected_session_mcp_servers": [self.steward_mcp_server()],
        });
        if let Some(workspace) = request.workspace.filter(|value| !value.trim().is_empty()) {
            extra["workspace"] = serde_json::Value::String(workspace);
        }
        let conversation = self
            .conversation
            .create(
                user_id,
                CreateConversationRequest {
                    r#type: None,
                    name: Some("大管家".to_owned()),
                    model: None,
                    assistant: Some(AssistantConversationRequest {
                        id: assistant_id.clone(),
                        locale: Some("zh-CN".to_owned()),
                        conversation_overrides: None,
                    }),
                    source: Some(ConversationSource::Aionui),
                    channel_chat_id: None,
                    extra,
                },
            )
            .await?;
        let conversation = self.pin_steward_name(user_id, &conversation.id).await?;
        let now = now_ms();
        self.repo
            .upsert_profile(&StewardProfileRow {
                user_id: user_id.to_owned(),
                conversation_id: Some(conversation.id.clone()),
                assistant_id: Some(assistant_id.clone()),
                created_at: now,
                updated_at: now,
            })
            .await?;
        Ok(profile_response(&conversation, assistant_id))
    }

    pub async fn profile(&self, user_id: &str) -> Result<Option<StewardProfileResponse>, ConversationError> {
        let Some(profile) = self.repo.get_profile(user_id).await? else {
            return Ok(None);
        };
        let Some(conversation_id) = profile.conversation_id else {
            return Ok(None);
        };
        let Ok(conversation) = self.conversation.get(user_id, &conversation_id).await else {
            return Ok(None);
        };
        self.ensure_steward_control_snapshot(user_id, &conversation).await?;
        Ok(Some(profile_response(
            &conversation,
            profile
                .assistant_id
                .unwrap_or_else(|| DEFAULT_STEWARD_ASSISTANT_ID.to_owned()),
        )))
    }

    pub async fn switch_assistant(
        &self,
        user_id: &str,
        request: SwitchStewardAssistantRequest,
    ) -> Result<StewardProfileResponse, ConversationError> {
        let _bootstrap_guard = self.bootstrap_lock.lock().await;
        let assistant_id = request.assistant_id.trim();
        if assistant_id.is_empty() {
            return Err(ConversationError::bad_request("assistant_id must not be empty"));
        }
        let profile = self
            .repo
            .get_profile(user_id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason("Steward profile not found"))?;
        let conversation_id = profile
            .conversation_id
            .clone()
            .ok_or_else(|| ConversationError::not_found_reason("Steward conversation not found"))?;
        if profile.assistant_id.as_deref() == Some(assistant_id) {
            return self
                .profile(user_id)
                .await?
                .ok_or_else(|| ConversationError::not_found_reason("Steward conversation not found"));
        }

        let conversation = self
            .conversation
            .switch_assistant(user_id, &conversation_id, assistant_id, &self.task_manager)
            .await?;
        self.ensure_steward_control_snapshot(user_id, &conversation).await?;
        self.repo
            .upsert_profile(&StewardProfileRow {
                user_id: user_id.to_owned(),
                conversation_id: Some(conversation_id),
                assistant_id: Some(assistant_id.to_owned()),
                created_at: profile.created_at,
                updated_at: now_ms(),
            })
            .await?;
        Ok(profile_response(&conversation, assistant_id.to_owned()))
    }

    pub async fn is_steward_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<bool, ConversationError> {
        Ok(self
            .profile(user_id)
            .await?
            .is_some_and(|profile| profile.conversation_id == conversation_id))
    }

    pub fn fixed_slash_commands() -> Vec<SlashCommandItem> {
        [
            ("/help", "查看大管家机器指令"),
            ("/tasks", "直接列出任务和未纳管会话"),
            ("/status", "直接查询任务与 Leader 运行状态，可追加任务名"),
            ("/workers", "直接查询专家团成员状态，可追加任务名"),
            ("/resume", "恢复已有任务的主会话，可追加任务名"),
            ("/archive", "归档指定任务及其主会话，需要任务名"),
            ("/restore", "恢复指定的已归档任务，需要任务名"),
            ("/ask", "询问指定任务的 Leader 并等待回复：/ask 任务名 问题"),
        ]
        .into_iter()
        .map(|(command, description)| SlashCommandItem {
            command: command.to_owned(),
            description: description.to_owned(),
            completion_behavior: None,
            empty_turn_tip_code: None,
            empty_turn_tip_params: None,
        })
        .collect()
    }

    pub async fn try_execute_command_for_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        content: &str,
    ) -> Result<StewardCommandResponse, ConversationError> {
        if !self.is_steward_conversation(user_id, conversation_id).await? {
            return Ok(unhandled_command_response());
        }
        self.try_execute_command(user_id, content).await
    }

    pub async fn try_execute_command(
        &self,
        user_id: &str,
        content: &str,
    ) -> Result<StewardCommandResponse, ConversationError> {
        let Some(command) = parse_steward_command(content) else {
            return Ok(unhandled_command_response());
        };
        let profile = match self.profile(user_id).await? {
            Some(profile) => profile,
            None => self.bootstrap(user_id, BootstrapStewardRequest::default()).await?,
        };
        let command_name = command.name().to_owned();
        let (text, task_id, target_conversation_id) = self.execute_parsed_command(user_id, command).await?;
        let executed_at = now_ms();
        let command_id = format!("steward-command-{}", generate_short_id());
        let user_msg_id = format!("steward-user-{}", generate_short_id());
        let response_msg_id = format!("steward-response-{}", generate_short_id());
        self.persist_command_exchange(
            user_id,
            &profile.conversation_id,
            content.trim(),
            &text,
            &command_id,
            &user_msg_id,
            &response_msg_id,
            executed_at,
        )
        .await?;
        tracing::info!(
            user_id,
            command = %command_name,
            task_id = task_id.as_deref().unwrap_or(""),
            target_conversation_id = target_conversation_id.as_deref().unwrap_or(""),
            outcome = "handled",
            "steward machine command executed"
        );
        Ok(StewardCommandResponse {
            handled: true,
            command: Some(command_name),
            text: Some(text),
            task_id,
            conversation_id: target_conversation_id,
            user_msg_id: Some(user_msg_id),
            response_msg_id: Some(response_msg_id),
            executed_at,
        })
    }

    async fn execute_parsed_command(
        &self,
        user_id: &str,
        command: ParsedStewardCommand,
    ) -> Result<(String, Option<String>, Option<String>), ConversationError> {
        match command {
            ParsedStewardCommand::Help => Ok((fixed_command_help(), None, None)),
            ParsedStewardCommand::Tasks => Ok((format_overview(&self.overview(user_id).await?), None, None)),
            ParsedStewardCommand::Status(target) => {
                let tasks = self.list_tasks(user_id, None, None, 200).await?;
                if target.is_none() {
                    let open = tasks
                        .iter()
                        .filter(|task| task.lifecycle == StewardTaskLifecycle::Open)
                        .collect::<Vec<_>>();
                    if open.len() != 1 {
                        return Ok((format_task_status_list(&tasks), None, None));
                    }
                }
                match resolve_command_task(tasks, target.as_deref()) {
                    CommandTaskLookup::Found(task) => {
                        let conversation_id = primary_session(&task).map(|session| session.conversation_id.clone());
                        let text = self.format_task_status(user_id, &task).await?;
                        Ok((text, Some(task.id), conversation_id))
                    }
                    CommandTaskLookup::Message(text) => Ok((text, None, None)),
                }
            }
            ParsedStewardCommand::Workers(target) => {
                let tasks = self.list_tasks(user_id, None, None, 200).await?;
                match resolve_command_task(tasks, target.as_deref()) {
                    CommandTaskLookup::Found(task) => {
                        let conversation_id = primary_session(&task).map(|session| session.conversation_id.clone());
                        let text = self.format_task_workers(user_id, &task).await?;
                        Ok((text, Some(task.id), conversation_id))
                    }
                    CommandTaskLookup::Message(text) => Ok((text, None, None)),
                }
            }
            ParsedStewardCommand::Resume(target) => {
                let tasks = self.list_tasks(user_id, None, None, 200).await?;
                match resolve_command_task(tasks, target.as_deref()) {
                    CommandTaskLookup::Found(task) => {
                        let resumed = self
                            .resume(user_id, &task.id, ResumeStewardTaskRequest { restart: false })
                            .await?;
                        let conversation_id = primary_session(&resumed).map(|session| session.conversation_id.clone());
                        Ok((
                            format!(
                                "已恢复任务「{}」的原主会话。当前执行状态：{}。",
                                resumed.title,
                                execution_state_label(&resumed.execution_state)
                            ),
                            Some(resumed.id),
                            conversation_id,
                        ))
                    }
                    CommandTaskLookup::Message(text) => Ok((text, None, None)),
                }
            }
            ParsedStewardCommand::Archive(target) => self.set_task_archived(user_id, &target, true).await,
            ParsedStewardCommand::Restore(target) => self.set_task_archived(user_id, &target, false).await,
            ParsedStewardCommand::Ask { target, question } => {
                let tasks = self.list_tasks(user_id, None, None, 200).await?;
                match resolve_command_task(tasks, target.as_deref()) {
                    CommandTaskLookup::Found(task) => {
                        let inquiry = self
                            .ask(user_id, &task.id, AskStewardTaskRequest { content: question })
                            .await?;
                        Ok((
                            format!("Leader 回复：\n{}", inquiry.reply),
                            Some(task.id),
                            Some(inquiry.conversation_id),
                        ))
                    }
                    CommandTaskLookup::Message(text) => Ok((text, None, None)),
                }
            }
        }
    }

    async fn format_task_status(&self, user_id: &str, task: &StewardTaskResponse) -> Result<String, ConversationError> {
        let leader = match primary_session(task) {
            Some(session) => Some(self.conversation.get(user_id, &session.conversation_id).await?),
            None => None,
        };
        let worker_count = match leader.as_ref() {
            Some(leader) => self.team_member_conversations(user_id, leader).await?.len(),
            None => 0,
        };
        let mut lines = vec![
            format!("任务：{}", task.title),
            format!("任务状态：{}", lifecycle_label(&task.lifecycle)),
            format!("执行状态：{}", execution_state_label(&task.execution_state)),
        ];
        if let Some(leader) = leader {
            lines.push(format!(
                "Leader：{}（{}）",
                leader.name,
                conversation_runtime_label(leader.runtime.as_ref(), leader.status)
            ));
        } else {
            lines.push("Leader：未绑定".to_owned());
        }
        lines.push(format!("专家团 Worker：{} 个", worker_count));
        if let Some(summary) = task
            .progress_summary
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(format!("最近进展：{summary}"));
        }
        if let Some(next_action) = task.next_action.as_deref().filter(|value| !value.trim().is_empty()) {
            lines.push(format!("下一步：{next_action}"));
        }
        if !task.blockers.is_empty() {
            lines.push(format!("阻塞：{}", task.blockers.join("；")));
        }
        Ok(lines.join("\n"))
    }

    async fn format_task_workers(
        &self,
        user_id: &str,
        task: &StewardTaskResponse,
    ) -> Result<String, ConversationError> {
        let Some(primary) = primary_session(task) else {
            return Ok(format!("任务「{}」尚未绑定 Leader。", task.title));
        };
        let leader = self.conversation.get(user_id, &primary.conversation_id).await?;
        let workers = self.team_member_conversations(user_id, &leader).await?;
        let mut lines = vec![format!(
            "Leader：{}（{}）",
            leader.name,
            conversation_runtime_label(leader.runtime.as_ref(), leader.status)
        )];
        if workers.is_empty() {
            lines.push("Worker：无".to_owned());
        } else {
            lines.push(format!("Worker：{} 个", workers.len()));
            lines.extend(workers.into_iter().map(|worker| {
                format!(
                    "- {}（{}）",
                    worker.name,
                    conversation_runtime_label(worker.runtime.as_ref(), worker.status)
                )
            }));
        }
        Ok(lines.join("\n"))
    }

    async fn team_member_conversations(
        &self,
        user_id: &str,
        leader: &ConversationResponse,
    ) -> Result<Vec<ConversationResponse>, ConversationError> {
        let Some(team_id) = leader.extra.get("embedded_team_id").and_then(serde_json::Value::as_str) else {
            return Ok(Vec::new());
        };
        let conversations = self
            .conversation
            .list(
                user_id,
                ListConversationsQuery {
                    cursor: None,
                    limit: Some(200),
                    source: None,
                    cron_job_id: None,
                    pinned: None,
                },
            )
            .await?;
        let mut workers = Vec::new();
        for candidate in conversations.items.into_iter().filter(|conversation| {
            conversation.extra.get("teamId").and_then(serde_json::Value::as_str) == Some(team_id)
                && conversation.extra.get("role").and_then(serde_json::Value::as_str) == Some("teammate")
        }) {
            workers.push(self.conversation.get(user_id, &candidate.id).await?);
        }
        workers.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(workers)
    }

    async fn set_task_archived(
        &self,
        user_id: &str,
        target: &str,
        archived: bool,
    ) -> Result<(String, Option<String>, Option<String>), ConversationError> {
        let tasks = self.list_tasks(user_id, None, None, 200).await?;
        let task = match resolve_command_task(tasks, Some(target)) {
            CommandTaskLookup::Found(task) => task,
            CommandTaskLookup::Message(text) => return Ok((text, None, None)),
        };
        let conversation_id = self.primary_conversation(&task.id).await?;
        let conversation = self.conversation.get(user_id, &conversation_id).await?;
        let archive_target = archive_target_for_conversation(&conversation)?;
        let affected_scope = if matches!(archive_target, StewardArchiveTarget::Team(_)) {
            "团队会话"
        } else {
            "主会话"
        };
        let control_port = self.control_port.get().ok_or_else(|| {
            ConversationError::internal("Steward conversation controls are unavailable in this runtime")
        })?;
        control_port
            .set_archived(user_id, archive_target, archived)
            .await
            .map_err(|reason| ConversationError::internal(format!("Steward conversation control failed: {reason}")))?;
        let mut row = self.required_task(user_id, &task.id).await?;
        row.lifecycle = if archived { "archived" } else { "open" }.to_owned();
        if archived {
            row.execution_state = "paused".to_owned();
        }
        row.updated_at = now_ms();
        self.repo.update_task(&row).await?;
        Ok((
            format!(
                "已{}任务「{}」及其{}。",
                if archived { "归档" } else { "恢复" },
                task.title,
                affected_scope
            ),
            Some(task.id),
            Some(conversation_id),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_command_exchange(
        &self,
        user_id: &str,
        conversation_id: &str,
        input: &str,
        output: &str,
        command_id: &str,
        user_msg_id: &str,
        response_msg_id: &str,
        executed_at: i64,
    ) -> Result<(), ConversationError> {
        let user_message = MessageRow {
            id: user_msg_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            msg_id: Some(user_msg_id.to_owned()),
            r#type: "text".to_owned(),
            content: serde_json::json!({ "content": input }).to_string(),
            position: Some("right".to_owned()),
            status: Some("finish".to_owned()),
            hidden: false,
            created_at: executed_at,
            backend_turn_id: None,
        };
        self.conversation.insert_raw_message(user_id, &user_message).await?;
        let response_message = MessageRow {
            id: response_msg_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            msg_id: Some(response_msg_id.to_owned()),
            r#type: "text".to_owned(),
            content: serde_json::json!({ "content": output }).to_string(),
            position: Some("left".to_owned()),
            status: Some("finish".to_owned()),
            hidden: false,
            created_at: executed_at.saturating_add(1),
            backend_turn_id: Some(command_id.to_owned()),
        };
        self.conversation.insert_raw_message(user_id, &response_message).await
    }

    pub async fn create_task(
        &self,
        user_id: &str,
        request: CreateStewardTaskRequest,
    ) -> Result<StewardTaskResponse, ConversationError> {
        let title = required_text("title", request.title)?;
        let objective = required_text("objective", request.objective)?;
        require_json_object("permission_policy", &request.permission_policy)?;
        require_json_object("budget_policy", &request.budget_policy)?;
        if let Some(conversation_id) = request.conversation_id.as_deref() {
            // Fail before persisting the task when the requested binding is
            // missing or belongs to another user.
            self.conversation.get(user_id, conversation_id).await?;
        }
        let now = now_ms();
        let row = StewardTaskRow {
            id: generate_short_id(),
            user_id: user_id.to_owned(),
            title,
            objective,
            lifecycle: "open".to_owned(),
            execution_state: if request.conversation_id.is_some() {
                "idle".to_owned()
            } else {
                "unassigned".to_owned()
            },
            priority: request.priority,
            progress_summary: None,
            next_action: None,
            blockers: "[]".to_owned(),
            project_id: request.project_id,
            folder_id: request.folder_id,
            workspace: request.workspace,
            permission_policy: json_object_or_empty(request.permission_policy).to_string(),
            budget_policy: json_object_or_empty(request.budget_policy).to_string(),
            created_at: now,
            updated_at: now,
        };
        self.repo.create_task(&row).await?;
        self.append_event(&row.id, "user", "task_created", serde_json::json!({}))
            .await?;
        if let Some(conversation_id) = request.conversation_id {
            self.bind_existing_session(user_id, &row.id, &conversation_id, StewardSessionRole::Primary)
                .await?;
        }
        self.task_response(user_id, row, false).await
    }

    pub async fn update_task(
        &self,
        user_id: &str,
        task_id: &str,
        request: UpdateStewardTaskRequest,
    ) -> Result<StewardTaskResponse, ConversationError> {
        let mut row = self.required_task(user_id, task_id).await?;
        if let Some(title) = request.title {
            row.title = required_text("title", title)?;
        }
        if let Some(objective) = request.objective {
            row.objective = required_text("objective", objective)?;
        }
        if let Some(lifecycle) = request.lifecycle {
            row.lifecycle = lifecycle.as_str().to_owned();
        }
        if let Some(execution_state) = request.execution_state {
            row.execution_state = execution_state.as_str().to_owned();
        }
        if let Some(priority) = request.priority {
            row.priority = priority;
        }
        if let Some(value) = request.progress_summary {
            row.progress_summary = nonempty_option(value);
        }
        if let Some(value) = request.next_action {
            row.next_action = nonempty_option(value);
        }
        if let Some(blockers) = request.blockers {
            row.blockers = serde_json::to_string(&blockers)
                .map_err(|error| ConversationError::internal(format!("serialize blockers: {error}")))?;
        }
        if let Some(value) = request.project_id {
            row.project_id = nonempty_option(value);
        }
        if let Some(value) = request.folder_id {
            row.folder_id = nonempty_option(value);
        }
        if let Some(value) = request.workspace {
            row.workspace = nonempty_option(value);
        }
        if let Some(value) = request.permission_policy {
            require_json_object("permission_policy", &value)?;
            row.permission_policy = value.to_string();
        }
        if let Some(value) = request.budget_policy {
            require_json_object("budget_policy", &value)?;
            row.budget_policy = value.to_string();
        }
        row.updated_at = now_ms();
        self.repo.update_task(&row).await?;
        self.append_event(
            task_id,
            "steward",
            "task_updated",
            serde_json::json!({
                "lifecycle": row.lifecycle,
                "execution_state": row.execution_state,
            }),
        )
        .await?;
        self.task_response(user_id, row, true).await
    }

    pub async fn get_task(&self, user_id: &str, task_id: &str) -> Result<StewardTaskResponse, ConversationError> {
        self.reconcile_runtime_states(user_id).await?;
        let row = self.required_task(user_id, task_id).await?;
        self.task_response(user_id, row, true).await
    }

    pub async fn list_tasks(
        &self,
        user_id: &str,
        query: Option<String>,
        lifecycle: Option<StewardTaskLifecycle>,
        limit: u32,
    ) -> Result<Vec<StewardTaskResponse>, ConversationError> {
        let rows = self
            .repo
            .list_tasks(
                user_id,
                &StewardTaskFilters {
                    query,
                    lifecycle: lifecycle.map(|value| value.as_str().to_owned()),
                    limit,
                },
            )
            .await?;
        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            tasks.push(self.task_response(user_id, row, false).await?);
        }
        Ok(tasks)
    }

    pub async fn overview(&self, user_id: &str) -> Result<StewardOverviewResponse, ConversationError> {
        self.reconcile_runtime_states(user_id).await?;
        let tasks = self.list_tasks(user_id, None, None, 200).await?;
        let profile = self.profile(user_id).await?;
        let bound_conversation_ids: HashSet<String> = tasks
            .iter()
            .flat_map(|task| task.sessions.iter().map(|session| session.conversation_id.clone()))
            .collect();
        let conversations = self
            .conversation
            .list(
                user_id,
                ListConversationsQuery {
                    cursor: None,
                    limit: Some(200),
                    source: None,
                    cron_job_id: None,
                    pinned: None,
                },
            )
            .await?;
        let unregistered_conversations = conversations
            .items
            .into_iter()
            .filter(|conversation| {
                profile
                    .as_ref()
                    .is_none_or(|profile| profile.conversation_id != conversation.id)
                    && !is_steward_conversation(conversation)
                    && !is_team_worker_conversation(conversation)
                    && !bound_conversation_ids.contains(&conversation.id)
            })
            .map(unregistered_conversation_response)
            .collect();
        Ok(StewardOverviewResponse {
            profile,
            open_tasks: tasks
                .iter()
                .filter(|task| task.lifecycle == StewardTaskLifecycle::Open)
                .count(),
            running_tasks: tasks
                .iter()
                .filter(|task| task.execution_state == StewardExecutionState::Running)
                .count(),
            waiting_tasks: tasks
                .iter()
                .filter(|task| {
                    matches!(
                        task.execution_state,
                        StewardExecutionState::WaitingUser | StewardExecutionState::WaitingExternal
                    )
                })
                .count(),
            interrupted_tasks: tasks
                .iter()
                .filter(|task| {
                    matches!(
                        task.execution_state,
                        StewardExecutionState::Interrupted | StewardExecutionState::Failed
                    )
                })
                .count(),
            tasks,
            unregistered_conversations,
        })
    }

    pub async fn resolve(
        &self,
        user_id: &str,
        request: ResolveStewardTaskRequest,
    ) -> Result<Vec<StewardTaskCandidateResponse>, ConversationError> {
        let objective = required_text("objective", request.objective)?;
        let rows = self
            .repo
            .list_tasks(
                user_id,
                &StewardTaskFilters {
                    query: None,
                    lifecycle: None,
                    limit: 200,
                },
            )
            .await?;
        let mut candidates = Vec::new();
        for row in rows {
            let (score, evidence) = candidate_score(
                &objective,
                &row.title,
                &row.objective,
                request.project_id.as_deref(),
                row.project_id.as_deref(),
                request.workspace.as_deref(),
                row.workspace.as_deref(),
            );
            if score > 0 {
                let primary = self
                    .repo
                    .list_sessions(&row.id)
                    .await?
                    .into_iter()
                    .find(|binding| binding.role == "primary")
                    .map(|binding| binding.conversation_id);
                candidates.push(StewardTaskCandidateResponse {
                    task_id: Some(row.id),
                    conversation_id: primary,
                    title: row.title,
                    score,
                    evidence,
                    lifecycle: Some(parse_lifecycle(&row.lifecycle)?),
                    execution_state: Some(parse_execution_state(&row.execution_state)?),
                });
            }
        }

        let conversations = self
            .conversation
            .list(
                user_id,
                ListConversationsQuery {
                    cursor: None,
                    limit: Some(200),
                    source: None,
                    cron_job_id: None,
                    pinned: None,
                },
            )
            .await?;
        for conversation in conversations.items {
            if is_steward_conversation(&conversation) || is_team_worker_conversation(&conversation) {
                continue;
            }
            let workspace = conversation.extra.get("workspace").and_then(serde_json::Value::as_str);
            let (score, evidence) = candidate_score(
                &objective,
                &conversation.name,
                "",
                request.project_id.as_deref(),
                conversation.project_id.as_deref(),
                request.workspace.as_deref(),
                workspace,
            );
            if score > 0
                && !candidates
                    .iter()
                    .any(|candidate| candidate.conversation_id.as_deref() == Some(conversation.id.as_str()))
            {
                candidates.push(StewardTaskCandidateResponse {
                    task_id: None,
                    conversation_id: Some(conversation.id),
                    title: conversation.name,
                    score,
                    evidence,
                    lifecycle: None,
                    execution_state: None,
                });
            }
        }
        // Legacy conversations may have generic or auto-generated titles. A
        // bounded message-content search lets the steward still rediscover
        // work that predates the durable task registry.
        if let Ok(matches) = self
            .conversation
            .search_messages(
                user_id,
                SearchMessagesQuery {
                    keyword: objective.clone(),
                    page: Some(1),
                    page_size: Some(30),
                },
            )
            .await
        {
            for matched in matches.items {
                if matched
                    .conversation
                    .extra
                    .get("steward")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                    || candidates
                        .iter()
                        .any(|candidate| candidate.conversation_id.as_deref() == Some(matched.conversation.id.as_str()))
                {
                    continue;
                }
                candidates.push(StewardTaskCandidateResponse {
                    task_id: None,
                    conversation_id: Some(matched.conversation.id),
                    title: matched.conversation.name,
                    score: 45,
                    evidence: vec!["message_content".to_owned()],
                    lifecycle: None,
                    execution_state: None,
                });
            }
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.score));
        candidates.truncate(request.limit.unwrap_or(10).clamp(1, 50) as usize);
        Ok(candidates)
    }

    pub async fn bind_session(
        &self,
        user_id: &str,
        task_id: &str,
        request: BindStewardTaskSessionRequest,
    ) -> Result<StewardTaskResponse, ConversationError> {
        let task = self.required_task(user_id, task_id).await?;
        let is_primary = request.role == StewardSessionRole::Primary;
        if is_primary && !request.replace_primary {
            let requested_conversation_id = request.conversation_id.as_deref();
            if let Some(existing) = self
                .repo
                .list_sessions(task_id)
                .await?
                .into_iter()
                .find(|binding| binding.role == "primary")
                && requested_conversation_id != Some(existing.conversation_id.as_str())
            {
                return Err(ConversationError::Busy {
                    reason: format!(
                        "task already has primary conversation {}; explicit replace_primary=true is required",
                        existing.conversation_id
                    ),
                });
            }
        }
        let conversation_id = match request.conversation_id {
            Some(id) => {
                self.conversation.get(user_id, &id).await?;
                id
            }
            None => {
                let assistant_id = request
                    .assistant_id
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        ConversationError::bad_request("assistant_id is required when creating a session")
                    })?;
                let workspace = request.workspace.or_else(|| task.workspace.clone());
                let mut extra = serde_json::json!({
                    "steward_task_id": task.id,
                    "preset_context": TASK_LEADER_PROMPT,
                });
                if let Some(workspace) = workspace.filter(|value| !value.trim().is_empty()) {
                    extra["workspace"] = serde_json::Value::String(workspace);
                }
                self.conversation
                    .create(
                        user_id,
                        CreateConversationRequest {
                            r#type: None,
                            name: Some(
                                request
                                    .conversation_name
                                    .filter(|value| !value.trim().is_empty())
                                    .unwrap_or_else(|| task.title.clone()),
                            ),
                            model: None,
                            assistant: Some(AssistantConversationRequest {
                                id: assistant_id,
                                locale: Some("zh-CN".to_owned()),
                                conversation_overrides: None,
                            }),
                            source: Some(ConversationSource::Aionui),
                            channel_chat_id: None,
                            extra,
                        },
                    )
                    .await?
                    .id
            }
        };
        self.bind_existing_session(user_id, task_id, &conversation_id, request.role)
            .await?;
        let mut task = self.required_task(user_id, task_id).await?;
        if is_primary && task.execution_state == "unassigned" {
            task.execution_state = "idle".to_owned();
            task.updated_at = now_ms();
            self.repo.update_task(&task).await?;
        }
        self.task_response(user_id, task, true).await
    }

    pub async fn dispatch(
        &self,
        user_id: &str,
        task_id: &str,
        request: DispatchStewardTaskRequest,
    ) -> Result<SendMessageResponse, ConversationError> {
        let content = required_text("content", request.content)?;
        let dispatched_at = now_ms();
        let mut task = self.required_task(user_id, task_id).await?;
        let conversation_id = self.primary_conversation(task_id).await?;
        let response = self
            .conversation
            .send_message(
                user_id,
                &conversation_id,
                SendMessageRequest {
                    content,
                    files: Vec::new(),
                    sessions: Vec::new(),
                    inject_skills: Vec::new(),
                    hidden: false,
                },
                &self.task_manager,
            )
            .await?;
        task.execution_state = "running".to_owned();
        task.updated_at = now_ms();
        self.repo.update_task(&task).await?;
        self.append_event(
            task_id,
            "steward",
            "task_dispatched",
            serde_json::json!({"conversation_id": conversation_id, "turn_id": response.turn_id}),
        )
        .await?;
        if self
            .wait_for_dispatch_activity(user_id, &conversation_id, dispatched_at, DISPATCH_START_PROBE_TIMEOUT)
            .await?
        {
            self.append_event(
                task_id,
                "runtime",
                "dispatch_started",
                serde_json::json!({"conversation_id": conversation_id, "msg_id": response.msg_id}),
            )
            .await?;
            return Ok(response);
        }

        tracing::warn!(
            task_id,
            conversation_id,
            msg_id = %response.msg_id,
            "steward dispatch was accepted but no execution activity was observed; recovering the same primary"
        );
        if let Err(error) = self
            .conversation
            .ensure_runtime(user_id, &conversation_id, &self.task_manager)
            .await
        {
            task.execution_state = "failed".to_owned();
            task.blockers = serde_json::to_string(&vec![
                "Primary conversation could not be recovered after accepting the message".to_owned(),
            ])
            .map_err(|serialize_error| ConversationError::internal(format!("serialize blockers: {serialize_error}")))?;
            task.updated_at = now_ms();
            self.repo.update_task(&task).await?;
            self.append_event(
                task_id,
                "runtime",
                "dispatch_recovery_failed",
                serde_json::json!({"conversation_id": conversation_id, "msg_id": response.msg_id}),
            )
            .await?;
            tracing::error!(
                task_id,
                conversation_id,
                msg_id = %response.msg_id,
                error = %error,
                "steward dispatch recovery failed"
            );
            return Err(error);
        }
        self.append_event(
            task_id,
            "runtime",
            "dispatch_recovery_requested",
            serde_json::json!({"conversation_id": conversation_id, "msg_id": response.msg_id}),
        )
        .await?;

        if self
            .wait_for_dispatch_activity(
                user_id,
                &conversation_id,
                dispatched_at,
                DISPATCH_START_RECOVERY_TIMEOUT,
            )
            .await?
        {
            self.append_event(
                task_id,
                "runtime",
                "dispatch_started_after_recovery",
                serde_json::json!({"conversation_id": conversation_id, "msg_id": response.msg_id}),
            )
            .await?;
            return Ok(response);
        }

        task.execution_state = "failed".to_owned();
        task.blockers = serde_json::to_string(&vec![
            "Primary conversation accepted the message but did not start after one recovery attempt".to_owned(),
        ])
        .map_err(|error| ConversationError::internal(format!("serialize blockers: {error}")))?;
        task.updated_at = now_ms();
        self.repo.update_task(&task).await?;
        self.append_event(
            task_id,
            "runtime",
            "dispatch_start_timeout",
            serde_json::json!({"conversation_id": conversation_id, "msg_id": response.msg_id}),
        )
        .await?;
        tracing::error!(
            task_id,
            conversation_id,
            msg_id = %response.msg_id,
            "steward dispatch did not start after one recovery attempt"
        );
        Err(ConversationError::Timeout {
            reason: format!(
                "primary conversation {conversation_id} did not start after one recovery attempt; no replacement session was created"
            ),
        })
    }

    pub async fn ask(
        &self,
        user_id: &str,
        task_id: &str,
        request: AskStewardTaskRequest,
    ) -> Result<StewardTaskInquiryResponse, ConversationError> {
        let conversation_id = self.primary_conversation(task_id).await?;
        let asked_at = now_ms();
        let response = self
            .dispatch(
                user_id,
                task_id,
                DispatchStewardTaskRequest {
                    content: request.content,
                },
            )
            .await?;
        let (reply, replied_at) = self
            .wait_for_leader_reply(user_id, &conversation_id, asked_at, LEADER_REPLY_TIMEOUT)
            .await?;
        self.append_event(
            task_id,
            "runtime",
            "leader_reply_received",
            serde_json::json!({
                "conversation_id": conversation_id,
                "request_msg_id": response.msg_id,
                "replied_at": replied_at,
            }),
        )
        .await?;
        Ok(StewardTaskInquiryResponse {
            conversation_id,
            request_msg_id: response.msg_id,
            reply,
            replied_at,
        })
    }

    pub async fn resume(
        &self,
        user_id: &str,
        task_id: &str,
        request: ResumeStewardTaskRequest,
    ) -> Result<StewardTaskResponse, ConversationError> {
        let task = self.required_task(user_id, task_id).await?;
        let conversation_id = self.primary_conversation(task_id).await?;
        if request.restart {
            self.conversation
                .restart_runtime(user_id, &conversation_id, &self.task_manager)
                .await?;
        } else {
            self.conversation
                .ensure_runtime(user_id, &conversation_id, &self.task_manager)
                .await?;
        }
        self.append_event(
            task_id,
            "steward",
            if request.restart {
                "runtime_restarted"
            } else {
                "runtime_ensured"
            },
            serde_json::json!({"conversation_id": conversation_id}),
        )
        .await?;
        self.task_response(user_id, task, true).await
    }

    async fn wait_for_dispatch_activity(
        &self,
        user_id: &str,
        conversation_id: &str,
        dispatched_at: i64,
        timeout: Duration,
    ) -> Result<bool, ConversationError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let conversation = self.conversation.get(user_id, conversation_id).await?;
            if runtime_has_execution_activity(conversation.runtime.as_ref()) {
                return Ok(true);
            }
            let messages = self
                .conversation
                .list_messages(
                    user_id,
                    conversation_id,
                    ListMessagesQuery {
                        limit: Some(20),
                        before: None,
                        after: None,
                        anchor_message_id: None,
                        content_mode: None,
                    },
                )
                .await?;
            if messages.items.iter().any(|message| {
                message.created_at >= dispatched_at
                    && message.position == Some(MessagePosition::Left)
                    && message.backend_turn_id.is_some()
            }) {
                return Ok(true);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(DISPATCH_START_POLL_INTERVAL.min(deadline - now)).await;
        }
    }

    async fn wait_for_leader_reply(
        &self,
        user_id: &str,
        conversation_id: &str,
        asked_at: i64,
        timeout: Duration,
    ) -> Result<(String, i64), ConversationError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let messages = self
                .conversation
                .list_messages(
                    user_id,
                    conversation_id,
                    ListMessagesQuery {
                        limit: Some(50),
                        before: None,
                        after: None,
                        anchor_message_id: None,
                        content_mode: None,
                    },
                )
                .await?;
            if let Some((reply, replied_at)) = messages
                .items
                .iter()
                .filter_map(|message| leader_reply_after(message, asked_at))
                .max_by_key(|(_, replied_at)| *replied_at)
            {
                return Ok((reply, replied_at));
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(ConversationError::Timeout {
                    reason: format!(
                        "primary conversation {conversation_id} accepted the question but did not return a leader reply"
                    ),
                });
            }
            tokio::time::sleep(LEADER_REPLY_POLL_INTERVAL.min(deadline - now)).await;
        }
    }

    fn steward_mcp_server(&self) -> SessionMcpServer {
        SessionMcpServer {
            id: STEWARD_MCP_SERVER_NAME.to_owned(),
            name: STEWARD_MCP_SERVER_NAME.to_owned(),
            transport: SessionMcpTransport::Stdio {
                command: self.helper_binary.clone(),
                args: vec!["mcp-steward-stdio".to_owned()],
                env: Default::default(),
            },
        }
    }

    async fn ensure_steward_control_snapshot(
        &self,
        user_id: &str,
        conversation: &aionui_api_types::ConversationResponse,
    ) -> Result<(), ConversationError> {
        let expected = self.steward_mcp_server();
        self.conversation
            .update_extra_if_changed(
                user_id,
                &conversation.id,
                serde_json::json!({
                    "steward": true,
                    "preset_context": STEWARD_PROMPT,
                    "embedded_team": false,
                    "embedded_team_id": null,
                    "teamId": null,
                    "team_mcp_stdio_config": null,
                    "slot_id": null,
                    "role": null,
                    "mcp_server_ids": [],
                    "session_mcp_servers": [expected],
                    "mcp_servers": [STEWARD_MCP_SERVER_NAME],
                    "mcp_statuses": [{
                        "id": STEWARD_MCP_SERVER_NAME,
                        "name": STEWARD_MCP_SERVER_NAME,
                        "status": "loaded",
                    }],
                }),
            )
            .await?;
        Ok(())
    }

    async fn pin_steward_name(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<aionui_api_types::ConversationResponse, ConversationError> {
        self.conversation
            .update(
                user_id,
                conversation_id,
                UpdateConversationRequest {
                    name: Some("大管家".to_owned()),
                    name_source: Some("user".to_owned()),
                    pinned: None,
                    model: None,
                    extra: None,
                },
                &self.task_manager,
            )
            .await
    }

    async fn required_task(&self, user_id: &str, task_id: &str) -> Result<StewardTaskRow, ConversationError> {
        self.repo
            .get_task(user_id, task_id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Steward task '{task_id}' not found")))
    }

    async fn bind_existing_session(
        &self,
        user_id: &str,
        task_id: &str,
        conversation_id: &str,
        role: StewardSessionRole,
    ) -> Result<(), ConversationError> {
        self.required_task(user_id, task_id).await?;
        self.conversation.get(user_id, conversation_id).await?;
        let now = now_ms();
        self.repo
            .bind_session(&StewardTaskSessionRow {
                id: generate_short_id(),
                task_id: task_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
                role: role.as_str().to_owned(),
                created_at: now,
                detached_at: None,
            })
            .await?;
        self.append_event(
            task_id,
            "steward",
            "session_bound",
            serde_json::json!({"conversation_id": conversation_id, "role": role.as_str()}),
        )
        .await
    }

    async fn primary_conversation(&self, task_id: &str) -> Result<String, ConversationError> {
        self.repo
            .list_sessions(task_id)
            .await?
            .into_iter()
            .find(|binding| binding.role == "primary")
            .map(|binding| binding.conversation_id)
            .ok_or_else(|| ConversationError::bad_request("task has no primary session"))
    }

    async fn append_event(
        &self,
        task_id: &str,
        source: &str,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), ConversationError> {
        self.repo
            .append_event(&StewardTaskEventRow {
                id: generate_short_id(),
                task_id: task_id.to_owned(),
                source: source.to_owned(),
                event_type: event_type.to_owned(),
                payload: payload.to_string(),
                created_at: now_ms(),
            })
            .await?;
        Ok(())
    }

    /// Project volatile conversation runtime into the task execution axis.
    /// A finished process never completes the durable task: it becomes idle,
    /// while a terminal assistant error becomes interrupted. Lifecycle remains
    /// untouched until the steward or user explicitly closes the objective.
    async fn reconcile_runtime_states(&self, user_id: &str) -> Result<(), ConversationError> {
        let rows = self
            .repo
            .list_tasks(
                user_id,
                &StewardTaskFilters {
                    query: None,
                    lifecycle: Some("open".to_owned()),
                    limit: 200,
                },
            )
            .await?;
        for mut row in rows {
            let Some(primary) = self
                .repo
                .list_sessions(&row.id)
                .await?
                .into_iter()
                .find(|binding| binding.role == "primary")
            else {
                if row.execution_state != "unassigned" && row.execution_state != "interrupted" {
                    row.execution_state = "interrupted".to_owned();
                    row.updated_at = now_ms();
                    self.repo.update_task(&row).await?;
                    self.append_event(&row.id, "runtime", "primary_binding_missing", serde_json::json!({}))
                        .await?;
                }
                continue;
            };
            let Ok(conversation) = self.conversation.get(user_id, &primary.conversation_id).await else {
                if row.execution_state != "interrupted" {
                    row.execution_state = "interrupted".to_owned();
                    row.updated_at = now_ms();
                    self.repo.update_task(&row).await?;
                    self.append_event(
                        &row.id,
                        "runtime",
                        "primary_session_missing",
                        serde_json::json!({"conversation_id": primary.conversation_id}),
                    )
                    .await?;
                }
                continue;
            };
            let is_processing = conversation
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.is_processing || runtime.has_task);
            let next_state = if is_processing {
                Some("running")
            } else if row.execution_state == "running" {
                let terminal_error = self
                    .conversation
                    .list_messages(
                        user_id,
                        &primary.conversation_id,
                        ListMessagesQuery {
                            limit: Some(1),
                            before: None,
                            after: None,
                            anchor_message_id: None,
                            content_mode: None,
                        },
                    )
                    .await
                    .ok()
                    .and_then(|page| page.items.last().and_then(|message| message.status))
                    == Some(MessageStatus::Error);
                Some(if terminal_error { "interrupted" } else { "idle" })
            } else {
                None
            };
            if let Some(next_state) = next_state
                && row.execution_state != next_state
            {
                row.execution_state = next_state.to_owned();
                row.updated_at = now_ms();
                self.repo.update_task(&row).await?;
                self.append_event(
                    &row.id,
                    "runtime",
                    "execution_state_reconciled",
                    serde_json::json!({
                        "conversation_id": primary.conversation_id,
                        "execution_state": next_state,
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn task_response(
        &self,
        user_id: &str,
        row: StewardTaskRow,
        include_events: bool,
    ) -> Result<StewardTaskResponse, ConversationError> {
        let bindings = self.repo.list_sessions(&row.id).await?;
        let mut sessions = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let Ok(conversation) = self.conversation.get(user_id, &binding.conversation_id).await else {
                continue;
            };
            sessions.push(StewardTaskSessionResponse {
                id: binding.id,
                conversation_id: conversation.id,
                conversation_name: conversation.name,
                role: parse_session_role(&binding.role)?,
                status: conversation
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.task_status)
                    .or(Some(conversation.status)),
                workspace: conversation
                    .extra
                    .get("workspace")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                project_id: conversation.project_id,
                created_at: binding.created_at,
            });
        }
        let events = if include_events {
            self.repo
                .list_events(&row.id, 50)
                .await?
                .into_iter()
                .map(|event| StewardTaskEventResponse {
                    id: event.id,
                    source: event.source,
                    event_type: event.event_type,
                    payload: serde_json::from_str(&event.payload).unwrap_or(serde_json::Value::Null),
                    created_at: event.created_at,
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(StewardTaskResponse {
            id: row.id,
            title: row.title,
            objective: row.objective,
            lifecycle: parse_lifecycle(&row.lifecycle)?,
            execution_state: parse_execution_state(&row.execution_state)?,
            priority: row.priority,
            progress_summary: row.progress_summary,
            next_action: row.next_action,
            blockers: serde_json::from_str(&row.blockers).unwrap_or_default(),
            project_id: row.project_id,
            folder_id: row.folder_id,
            workspace: row.workspace,
            permission_policy: serde_json::from_str(&row.permission_policy).unwrap_or_else(|_| serde_json::json!({})),
            budget_policy: serde_json::from_str(&row.budget_policy).unwrap_or_else(|_| serde_json::json!({})),
            sessions,
            events,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

fn runtime_has_execution_activity(runtime: Option<&ConversationRuntimeSummary>) -> bool {
    runtime.is_some_and(|runtime| {
        runtime.is_processing
            || runtime.task_status == Some(ConversationStatus::Running)
            || runtime.pending_confirmations > 0
    })
}

fn leader_reply_after(message: &MessageResponse, asked_at: i64) -> Option<(String, i64)> {
    if message.created_at < asked_at
        || message.r#type != MessageType::Text
        || message.position != Some(MessagePosition::Left)
        || message.status != Some(MessageStatus::Finish)
        || message.hidden
        || message.backend_turn_id.is_none()
    {
        return None;
    }
    let reply = message
        .content
        .as_str()
        .or_else(|| message.content.get("content").and_then(serde_json::Value::as_str))?
        .trim();
    (!reply.is_empty()).then(|| (reply.to_owned(), message.created_at))
}

enum CommandTaskLookup {
    Found(Box<StewardTaskResponse>),
    Message(String),
}

fn unhandled_command_response() -> StewardCommandResponse {
    StewardCommandResponse {
        handled: false,
        command: None,
        text: None,
        task_id: None,
        conversation_id: None,
        user_msg_id: None,
        response_msg_id: None,
        executed_at: now_ms(),
    }
}

fn parse_steward_command(content: &str) -> Option<ParsedStewardCommand> {
    let input = content.trim();
    if input.is_empty() {
        return None;
    }
    if let Some(command) = input.strip_prefix('/') {
        let mut parts = command.splitn(2, char::is_whitespace);
        let name = parts.next()?.to_ascii_lowercase();
        let args = parts.next().unwrap_or_default().trim();
        return match name.as_str() {
            "help" | "帮助" => Some(ParsedStewardCommand::Help),
            "tasks" | "任务" => Some(ParsedStewardCommand::Tasks),
            "status" | "状态" => Some(ParsedStewardCommand::Status(nonempty_option(args.to_owned()))),
            "workers" | "成员" => Some(ParsedStewardCommand::Workers(nonempty_option(args.to_owned()))),
            "resume" | "继续" => Some(ParsedStewardCommand::Resume(nonempty_option(args.to_owned()))),
            "archive" | "归档" if !args.is_empty() => Some(ParsedStewardCommand::Archive(args.to_owned())),
            "restore" | "恢复" if !args.is_empty() => Some(ParsedStewardCommand::Restore(args.to_owned())),
            "ask" | "问" => parse_ask_arguments(args),
            _ => None,
        };
    }

    let normalized = input.trim_end_matches(['。', '？', '?']).trim();
    if matches!(normalized, "有哪些任务" | "任务列表" | "查看任务" | "当前任务") {
        return Some(ParsedStewardCommand::Tasks);
    }
    if matches!(normalized, "帮助" | "有哪些指令" | "有什么指令") {
        return Some(ParsedStewardCommand::Help);
    }
    if matches!(
        normalized,
        "leader现在在做什么" | "Leader现在在做什么" | "领导现在在做什么"
    ) {
        return Some(ParsedStewardCommand::Status(None));
    }
    for prefix in ["问下leader", "问一下leader", "问下 Leader", "问一下 Leader"] {
        if let Some(question) = normalized
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(ParsedStewardCommand::Ask {
                target: None,
                question: question.to_owned(),
            });
        }
    }
    for suffix in ["现在在做什么", "的状态"] {
        if let Some(target) = normalized
            .strip_suffix(suffix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(ParsedStewardCommand::Status(Some(target.to_owned())));
        }
    }
    for suffix in ["有哪些成员", "的成员", "的worker", "的 Worker"] {
        if let Some(target) = normalized
            .strip_suffix(suffix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(ParsedStewardCommand::Workers(Some(target.to_owned())));
        }
    }
    for prefix in ["继续任务", "继续"] {
        if let Some(target) = normalized
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(ParsedStewardCommand::Resume(Some(target.to_owned())));
        }
    }
    for prefix in ["归档任务", "归档"] {
        if let Some(target) = normalized
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(ParsedStewardCommand::Archive(target.to_owned()));
        }
    }
    for prefix in ["恢复归档任务", "恢复任务", "取消归档"] {
        if let Some(target) = normalized
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(ParsedStewardCommand::Restore(target.to_owned()));
        }
    }
    None
}

fn parse_ask_arguments(args: &str) -> Option<ParsedStewardCommand> {
    let mut parts = args.splitn(2, char::is_whitespace);
    let target = parts.next()?.trim();
    let question = parts.next()?.trim();
    if target.is_empty() || question.is_empty() {
        return None;
    }
    Some(ParsedStewardCommand::Ask {
        target: (!target.eq_ignore_ascii_case("leader") && target != "领导").then(|| target.to_owned()),
        question: question.to_owned(),
    })
}

fn fixed_command_help() -> String {
    [
        "大管家快速指令：",
        "/tasks — 列出任务和未纳管会话",
        "/status [任务名] — 查询任务与 Leader 状态",
        "/workers [任务名] — 查询团队成员状态",
        "/resume [任务名] — 恢复原主会话",
        "/archive <任务名> — 归档任务及主会话/团队",
        "/restore <任务名> — 恢复已归档任务",
        "/ask <任务名> <问题> — 询问 Leader 并等待回复",
        "不匹配快速指令的消息会继续交给大管家 AI。",
    ]
    .join("\n")
}

fn format_overview(overview: &StewardOverviewResponse) -> String {
    let mut lines = vec![format!(
        "任务概览：进行中 {}，执行中 {}，等待 {}，中断 {}。",
        overview.open_tasks, overview.running_tasks, overview.waiting_tasks, overview.interrupted_tasks
    )];
    if overview.tasks.is_empty() {
        lines.push("已登记任务：无".to_owned());
    } else {
        lines.push("已登记任务：".to_owned());
        lines.extend(overview.tasks.iter().map(|task| {
            format!(
                "- {} [{} / {}]",
                task.title,
                lifecycle_label(&task.lifecycle),
                execution_state_label(&task.execution_state)
            )
        }));
    }
    if !overview.unregistered_conversations.is_empty() {
        lines.push("未纳管会话：".to_owned());
        lines.extend(overview.unregistered_conversations.iter().map(|conversation| {
            format!(
                "- {} [{}]",
                conversation.conversation_name,
                conversation_status_label(conversation.status)
            )
        }));
    }
    lines.join("\n")
}

fn format_task_status_list(tasks: &[StewardTaskResponse]) -> String {
    let open = tasks
        .iter()
        .filter(|task| task.lifecycle == StewardTaskLifecycle::Open)
        .collect::<Vec<_>>();
    if open.is_empty() {
        return "当前没有进行中的已登记任务。可使用 /tasks 查看全部任务和未纳管会话。".to_owned();
    }
    let mut lines = vec!["当前有多个进行中任务，请指定任务名：".to_owned()];
    lines.extend(
        open.into_iter()
            .map(|task| format!("- {} [{}]", task.title, execution_state_label(&task.execution_state))),
    );
    lines.join("\n")
}

fn resolve_command_task(tasks: Vec<StewardTaskResponse>, target: Option<&str>) -> CommandTaskLookup {
    let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
        let mut open = tasks
            .into_iter()
            .filter(|task| task.lifecycle == StewardTaskLifecycle::Open)
            .collect::<Vec<_>>();
        return match open.len() {
            0 => CommandTaskLookup::Message(
                "当前没有进行中的已登记任务。可使用 /tasks 查看全部任务和未纳管会话。".to_owned(),
            ),
            1 => CommandTaskLookup::Found(Box::new(open.remove(0))),
            _ => CommandTaskLookup::Message(format_task_status_list(&open)),
        };
    };
    let needle = target.to_lowercase();
    let mut exact = tasks
        .iter()
        .filter(|task| {
            task.id.eq_ignore_ascii_case(target)
                || task.title.eq_ignore_ascii_case(target)
                || task.sessions.iter().any(|session| {
                    session.conversation_id.eq_ignore_ascii_case(target)
                        || session.conversation_name.eq_ignore_ascii_case(target)
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    if exact.len() == 1 {
        return CommandTaskLookup::Found(Box::new(exact.remove(0)));
    }
    let mut partial = tasks
        .into_iter()
        .filter(|task| {
            task.title.to_lowercase().contains(&needle)
                || task
                    .sessions
                    .iter()
                    .any(|session| session.conversation_name.to_lowercase().contains(&needle))
        })
        .collect::<Vec<_>>();
    match partial.len() {
        0 => CommandTaskLookup::Message(format!("没有找到任务「{target}」。可使用 /tasks 查看任务列表。")),
        1 => CommandTaskLookup::Found(Box::new(partial.remove(0))),
        _ => CommandTaskLookup::Message(format!(
            "任务名「{target}」不唯一，请指定：\n{}",
            partial
                .iter()
                .map(|task| format!("- {}", task.title))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

fn primary_session(task: &StewardTaskResponse) -> Option<&StewardTaskSessionResponse> {
    task.sessions
        .iter()
        .find(|session| session.role == StewardSessionRole::Primary)
        .or_else(|| task.sessions.first())
}

fn lifecycle_label(value: &StewardTaskLifecycle) -> &'static str {
    match value {
        StewardTaskLifecycle::Open => "进行中",
        StewardTaskLifecycle::Completed => "已完成",
        StewardTaskLifecycle::Cancelled => "已取消",
        StewardTaskLifecycle::Archived => "已归档",
    }
}

fn execution_state_label(value: &StewardExecutionState) -> &'static str {
    match value {
        StewardExecutionState::Unassigned => "未分配",
        StewardExecutionState::Running => "执行中",
        StewardExecutionState::WaitingUser => "等待用户",
        StewardExecutionState::WaitingExternal => "等待外部",
        StewardExecutionState::Paused => "已暂停",
        StewardExecutionState::Interrupted => "已中断",
        StewardExecutionState::Failed => "失败",
        StewardExecutionState::Idle => "空闲",
    }
}

fn conversation_runtime_label(
    runtime: Option<&ConversationRuntimeSummary>,
    persisted_status: ConversationStatus,
) -> &'static str {
    let Some(runtime) = runtime else {
        return conversation_status_label(persisted_status);
    };
    if runtime.is_processing {
        return "执行中";
    }
    match runtime.state {
        ConversationRuntimeStateKind::Idle => conversation_status_label(persisted_status),
        ConversationRuntimeStateKind::Starting => "启动中",
        ConversationRuntimeStateKind::Running => "运行中",
        ConversationRuntimeStateKind::Cancelling => "取消中",
        ConversationRuntimeStateKind::Restarting => "重启中",
        ConversationRuntimeStateKind::WaitingConfirmation => "等待确认",
    }
}

fn conversation_status_label(value: ConversationStatus) -> &'static str {
    match value {
        ConversationStatus::Pending => "等待中",
        ConversationStatus::Running => "运行中",
        ConversationStatus::Finished => "空闲",
    }
}

fn archive_target_for_conversation(
    conversation: &ConversationResponse,
) -> Result<StewardArchiveTarget, ConversationError> {
    if is_steward_conversation(conversation) {
        return Err(ConversationError::bad_request(
            "The steward conversation cannot be archived",
        ));
    }
    if is_team_worker_conversation(conversation) {
        return Err(ConversationError::bad_request(
            "An individual team worker cannot be archived; archive its leader instead",
        ));
    }
    if let Some(team_id) = conversation
        .extra
        .get("embedded_team_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(StewardArchiveTarget::Team(team_id.to_owned()));
    }
    Ok(StewardArchiveTarget::Conversation(conversation.id.clone()))
}

fn profile_response(
    conversation: &aionui_api_types::ConversationResponse,
    assistant_id: String,
) -> StewardProfileResponse {
    StewardProfileResponse {
        conversation_id: conversation.id.clone(),
        assistant_id,
        conversation_name: conversation.name.clone(),
        workspace: conversation
            .extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    }
}

fn is_steward_conversation(conversation: &aionui_api_types::ConversationResponse) -> bool {
    conversation.extra.get("steward").and_then(serde_json::Value::as_bool) == Some(true)
}

fn is_team_worker_conversation(conversation: &aionui_api_types::ConversationResponse) -> bool {
    match conversation.extra.get("role").and_then(serde_json::Value::as_str) {
        Some("lead") => false,
        Some("teammate") => true,
        _ => conversation.extra.get("teamId").is_some(),
    }
}

fn unregistered_conversation_response(
    conversation: aionui_api_types::ConversationResponse,
) -> StewardUnregisteredConversationResponse {
    let assistant_name = conversation.assistant.as_ref().map(|assistant| assistant.name.clone());
    let backend = conversation
        .assistant
        .as_ref()
        .map(|assistant| assistant.backend.clone())
        .or_else(|| {
            conversation
                .extra
                .get("backend")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let workspace = conversation
        .extra
        .get("workspace")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    StewardUnregisteredConversationResponse {
        conversation_id: conversation.id,
        conversation_name: conversation.name,
        status: conversation.status,
        assistant_name,
        backend,
        workspace,
        project_id: conversation.project_id,
        modified_at: conversation.modified_at,
    }
}

fn required_text(field: &str, value: String) -> Result<String, ConversationError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(ConversationError::bad_request(format!("{field} must not be empty")))
    } else {
        Ok(trimmed.to_owned())
    }
}

fn nonempty_option(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn require_json_object(field: &str, value: &serde_json::Value) -> Result<(), ConversationError> {
    if value.is_null() || value.is_object() {
        Ok(())
    } else {
        Err(ConversationError::bad_request(format!("{field} must be a JSON object")))
    }
}

fn json_object_or_empty(value: serde_json::Value) -> serde_json::Value {
    if value.is_null() { serde_json::json!({}) } else { value }
}

fn parse_lifecycle(value: &str) -> Result<StewardTaskLifecycle, ConversationError> {
    match value {
        "open" => Ok(StewardTaskLifecycle::Open),
        "completed" => Ok(StewardTaskLifecycle::Completed),
        "cancelled" => Ok(StewardTaskLifecycle::Cancelled),
        "archived" => Ok(StewardTaskLifecycle::Archived),
        other => Err(ConversationError::internal(format!(
            "invalid steward lifecycle: {other}"
        ))),
    }
}

fn parse_execution_state(value: &str) -> Result<StewardExecutionState, ConversationError> {
    match value {
        "unassigned" => Ok(StewardExecutionState::Unassigned),
        "running" => Ok(StewardExecutionState::Running),
        "waiting_user" => Ok(StewardExecutionState::WaitingUser),
        "waiting_external" => Ok(StewardExecutionState::WaitingExternal),
        "paused" => Ok(StewardExecutionState::Paused),
        "interrupted" => Ok(StewardExecutionState::Interrupted),
        "failed" => Ok(StewardExecutionState::Failed),
        "idle" => Ok(StewardExecutionState::Idle),
        other => Err(ConversationError::internal(format!(
            "invalid steward execution state: {other}"
        ))),
    }
}

fn parse_session_role(value: &str) -> Result<StewardSessionRole, ConversationError> {
    match value {
        "primary" => Ok(StewardSessionRole::Primary),
        "worker" => Ok(StewardSessionRole::Worker),
        "replacement" => Ok(StewardSessionRole::Replacement),
        "observer" => Ok(StewardSessionRole::Observer),
        other => Err(ConversationError::internal(format!(
            "invalid steward session role: {other}"
        ))),
    }
}

fn candidate_score(
    query: &str,
    title: &str,
    objective: &str,
    query_project: Option<&str>,
    candidate_project: Option<&str>,
    query_workspace: Option<&str>,
    candidate_workspace: Option<&str>,
) -> (i64, Vec<String>) {
    let query_lower = query.to_lowercase();
    let title_lower = title.to_lowercase();
    let objective_lower = objective.to_lowercase();
    let mut score = 0;
    let mut evidence = Vec::new();
    if query_lower == objective_lower || query_lower == title_lower {
        score += 100;
        evidence.push("exact_text".to_owned());
    } else if title_lower.contains(&query_lower)
        || objective_lower.contains(&query_lower)
        || query_lower.contains(&title_lower)
    {
        score += 60;
        evidence.push("text_substring".to_owned());
    } else {
        let tokens = query_lower.split_whitespace().filter(|token| token.len() >= 2);
        let overlap = tokens
            .filter(|token| title_lower.contains(token) || objective_lower.contains(token))
            .count() as i64;
        if overlap > 0 {
            score += (overlap * 12).min(48);
            evidence.push(format!("token_overlap:{overlap}"));
        }
    }
    if query_project.is_some() && query_project == candidate_project {
        score += 25;
        evidence.push("same_project".to_owned());
    }
    if query_workspace.is_some() && query_workspace == candidate_workspace {
        score += 30;
        evidence.push("same_workspace".to_owned());
    }
    (score, evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::ConversationRuntimeStateKind;

    #[test]
    fn candidate_scoring_explains_project_and_workspace_matches() {
        let (score, evidence) = candidate_score(
            "修复登录故障",
            "修复登录故障",
            "",
            Some("project-a"),
            Some("project-a"),
            Some("/repo"),
            Some("/repo"),
        );
        assert_eq!(score, 155);
        assert_eq!(evidence, vec!["exact_text", "same_project", "same_workspace"]);
    }

    #[test]
    fn dispatch_activity_requires_real_execution_evidence() {
        let mut runtime = ConversationRuntimeSummary {
            state: ConversationRuntimeStateKind::Idle,
            can_send_message: true,
            has_task: true,
            task_status: None,
            is_processing: false,
            pending_confirmations: 0,
            turn_id: None,
            supports_midturn_delivery: true,
        };
        assert!(!runtime_has_execution_activity(Some(&runtime)));

        runtime.is_processing = true;
        assert!(runtime_has_execution_activity(Some(&runtime)));

        runtime.is_processing = false;
        runtime.task_status = Some(ConversationStatus::Running);
        assert!(runtime_has_execution_activity(Some(&runtime)));

        runtime.task_status = None;
        runtime.pending_confirmations = 1;
        assert!(runtime_has_execution_activity(Some(&runtime)));
    }

    #[test]
    fn inquiry_reply_requires_new_completed_leader_text() {
        let asked_at = 200;
        let mut message = MessageResponse {
            id: "message-1".to_owned(),
            conversation_id: "conversation-1".to_owned(),
            msg_id: Some("message-1".to_owned()),
            r#type: MessageType::Text,
            content: serde_json::json!({"content": "正在处理登录问题"}),
            position: Some(MessagePosition::Left),
            status: Some(MessageStatus::Finish),
            hidden: false,
            created_at: 201,
            backend_turn_id: Some("turn-1".to_owned()),
        };
        assert_eq!(
            leader_reply_after(&message, asked_at),
            Some(("正在处理登录问题".to_owned(), 201))
        );

        message.created_at = 199;
        assert_eq!(leader_reply_after(&message, asked_at), None);
        message.created_at = 201;
        message.backend_turn_id = None;
        assert_eq!(leader_reply_after(&message, asked_at), None);
        message.backend_turn_id = Some("turn-1".to_owned());
        message.r#type = MessageType::ToolCall;
        assert_eq!(leader_reply_after(&message, asked_at), None);
    }

    #[test]
    fn fixed_command_parser_is_conservative_and_supports_common_chinese_phrases() {
        assert_eq!(parse_steward_command("有哪些任务？"), Some(ParsedStewardCommand::Tasks));
        assert_eq!(
            parse_steward_command("小说续写现在在做什么"),
            Some(ParsedStewardCommand::Status(Some("小说续写".to_owned())))
        );
        assert_eq!(
            parse_steward_command("/ask 小说续写 现在写到哪里了"),
            Some(ParsedStewardCommand::Ask {
                target: Some("小说续写".to_owned()),
                question: "现在写到哪里了".to_owned(),
            })
        );
        assert_eq!(
            parse_steward_command("问下leader 现在在做什么"),
            Some(ParsedStewardCommand::Ask {
                target: None,
                question: "现在在做什么".to_owned(),
            })
        );
        assert_eq!(parse_steward_command("帮我规划今天的工作"), None);
        assert_eq!(parse_steward_command("/archive"), None);
    }
}
