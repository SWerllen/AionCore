//! MCP stdio bridge injected only into the personal steward conversation.
//!
//! The bridge uses the short-lived conversation helper credential already
//! inherited by the agent process. It never stores a JWT or a long-lived
//! password in the conversation snapshot.

use std::process::ExitCode;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::Deserialize;
use serde_json::{Value, json};

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";

pub async fn run_steward_stdio() -> ExitCode {
    let env = match RuntimeEnv::from_env() {
        Ok(env) => env,
        Err(message) => {
            eprintln!("STEWARD_MCP_ENV_MISSING: {message}");
            return ExitCode::from(2);
        }
    };
    let server = StewardStdioServer {
        client: reqwest::Client::new(),
        env,
    };
    match server.serve(transport::io::stdio()).await {
        Ok(peer) => match peer.waiting().await {
            Ok(_) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("STEWARD_MCP_SESSION_FAILED: {error}");
                ExitCode::from(1)
            }
        },
        Err(error) => {
            eprintln!("STEWARD_MCP_SERVE_FAILED: {error}");
            ExitCode::from(1)
        }
    }
}

#[derive(Clone)]
struct RuntimeEnv {
    base_url: String,
    user_id: String,
    conversation_id: String,
    token: String,
}

impl RuntimeEnv {
    fn from_env() -> Result<Self, String> {
        let required = |name: &'static str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| name.to_owned())
        };
        Ok(Self {
            base_url: required(ENV_BASE_URL)?,
            user_id: required(ENV_USER_ID)?,
            conversation_id: required(ENV_CONVERSATION_ID)?,
            token: required(ENV_RUNTIME_TOKEN)?,
        })
    }

    fn headers(&self) -> Result<reqwest::header::HeaderMap, String> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("x-aionui-user-id", &self.user_id),
            ("x-aionui-conversation-id", &self.conversation_id),
            ("x-aionui-runtime-token", &self.token),
        ] {
            headers.insert(
                name,
                value
                    .parse()
                    .map_err(|_| format!("invalid runtime header value for {name}"))?,
            );
        }
        Ok(headers)
    }
}

#[derive(Clone)]
struct StewardStdioServer {
    client: reqwest::Client,
    env: RuntimeEnv,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SearchTasksParams {
    objective: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListTasksParams {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    lifecycle: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TaskIdParams {
    task_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateTaskParams {
    title: String,
    objective: String,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateTaskParams {
    task_id: String,
    #[serde(default)]
    lifecycle: Option<String>,
    #[serde(default)]
    execution_state: Option<String>,
    #[serde(default)]
    progress_summary: Option<String>,
    #[serde(default)]
    next_action: Option<String>,
    #[serde(default)]
    blockers: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateSessionParams {
    task_id: String,
    assistant_id: String,
    #[serde(default)]
    conversation_name: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    replace_primary: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct BindSessionParams {
    task_id: String,
    conversation_id: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    replace_primary: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DispatchTaskParams {
    task_id: String,
    content: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AskTaskParams {
    task_id: String,
    content: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ResumeTaskParams {
    task_id: String,
    #[serde(default)]
    restart: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ConversationIdParams {
    conversation_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListArchivedParams {
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
enum ConversationControlTarget {
    Conversation {
        conversation_id: String,
        conversation_name: String,
    },
    Team {
        conversation_id: String,
        conversation_name: String,
        team_id: String,
    },
}

#[tool_router]
impl StewardStdioServer {
    #[tool(
        name = "steward_overview",
        description = "Get the complete work overview: registered durable tasks plus unregistered top-level conversations. Team workers are excluded from the unregistered list. Use this before reporting what tasks exist, and report both groups."
    )]
    async fn overview(&self) -> CallToolResult {
        self.get("/api/steward/overview", &[]).await
    }

    #[tool(
        name = "steward_search_tasks",
        description = "Search durable tasks and legacy conversations before creating new work. Returns ranked candidates with explicit match evidence."
    )]
    async fn search_tasks(&self, Parameters(params): Parameters<SearchTasksParams>) -> CallToolResult {
        self.post(
            "/api/steward/tasks/resolve",
            json!({
                "objective": params.objective,
                "project_id": params.project_id,
                "workspace": params.workspace,
                "limit": params.limit,
            }),
        )
        .await
    }

    #[tool(
        name = "steward_list_tasks",
        description = "List durable tasks. lifecycle may be open, completed, cancelled, or archived."
    )]
    async fn list_tasks(&self, Parameters(params): Parameters<ListTasksParams>) -> CallToolResult {
        let mut query = Vec::new();
        if let Some(value) = params.query {
            query.push(("query", value));
        }
        if let Some(value) = params.lifecycle {
            query.push(("lifecycle", value));
        }
        if let Some(value) = params.limit {
            query.push(("limit", value.to_string()));
        }
        self.get("/api/steward/tasks", &query).await
    }

    #[tool(
        name = "steward_inspect_task",
        description = "Inspect one durable task, its bound sessions, recent events, blockers, progress, and next action."
    )]
    async fn inspect_task(&self, Parameters(params): Parameters<TaskIdParams>) -> CallToolResult {
        self.get(&format!("/api/steward/tasks/{}", params.task_id), &[]).await
    }

    #[tool(
        name = "steward_create_task",
        description = "Create a durable task only after steward_search_tasks found no suitable existing task. Optionally bind an existing conversation as primary."
    )]
    async fn create_task(&self, Parameters(params): Parameters<CreateTaskParams>) -> CallToolResult {
        self.post(
            "/api/steward/tasks",
            json!({
                "title": params.title,
                "objective": params.objective,
                "priority": params.priority.unwrap_or(0),
                "project_id": params.project_id,
                "workspace": params.workspace,
                "conversation_id": params.conversation_id,
            }),
        )
        .await
    }

    #[tool(
        name = "steward_update_task",
        description = "Update task lifecycle and execution state independently, plus its progress summary, next action, and blockers."
    )]
    async fn update_task(&self, Parameters(params): Parameters<UpdateTaskParams>) -> CallToolResult {
        let path = format!("/api/steward/tasks/{}", params.task_id);
        self.patch(
            &path,
            json!({
                "lifecycle": params.lifecycle,
                "execution_state": params.execution_state,
                "progress_summary": params.progress_summary,
                "next_action": params.next_action,
                "blockers": params.blockers,
            }),
        )
        .await
    }

    #[tool(
        name = "steward_list_assistants",
        description = "List actual enabled AionUi assistant IDs. Use one returned id when creating a task execution session; never guess an assistant id."
    )]
    async fn list_assistants(&self) -> CallToolResult {
        self.get("/api/assistants", &[]).await
    }

    #[tool(
        name = "steward_create_session",
        description = "Create and bind a primary execution session only when the task has no primary. Replacing an existing primary requires an explicit user request and replace_primary=true."
    )]
    async fn create_session(&self, Parameters(params): Parameters<CreateSessionParams>) -> CallToolResult {
        self.post(
            &format!("/api/steward/tasks/{}/sessions", params.task_id),
            json!({
                "assistant_id": params.assistant_id,
                "conversation_name": params.conversation_name,
                "workspace": params.workspace,
                "role": "primary",
                "replace_primary": params.replace_primary.unwrap_or(false),
            }),
        )
        .await
    }

    #[tool(
        name = "steward_bind_session",
        description = "Bind an existing owned conversation to a durable task. Replacing an existing primary requires an explicit user request and replace_primary=true."
    )]
    async fn bind_session(&self, Parameters(params): Parameters<BindSessionParams>) -> CallToolResult {
        self.post(
            &format!("/api/steward/tasks/{}/sessions", params.task_id),
            json!({
                "conversation_id": params.conversation_id,
                "role": params.role.unwrap_or_else(|| "primary".to_owned()),
                "replace_primary": params.replace_primary.unwrap_or(false),
            }),
        )
        .await
    }

    #[tool(
        name = "steward_dispatch_task",
        description = "Send work to the task's primary session and mark it running. The primary session decides whether to work solo or activate its embedded expert team."
    )]
    async fn dispatch_task(&self, Parameters(params): Parameters<DispatchTaskParams>) -> CallToolResult {
        self.post(
            &format!("/api/steward/tasks/{}/dispatch", params.task_id),
            json!({"content": params.content}),
        )
        .await
    }

    #[tool(
        name = "steward_ask_task",
        description = "Ask the task's primary leader a question and wait for its next completed text reply. Use this for status questions such as what the leader is doing; include the returned reply in the user-facing answer and never stop at 'message sent'."
    )]
    async fn ask_task(&self, Parameters(params): Parameters<AskTaskParams>) -> CallToolResult {
        self.post(
            &format!("/api/steward/tasks/{}/ask", params.task_id),
            json!({"content": params.content}),
        )
        .await
    }

    #[tool(
        name = "steward_resume_task",
        description = "Ensure the task's primary agent runtime is available after interruption. restart=false is the safe default; use restart=true only when a broken live runtime must be replaced."
    )]
    async fn resume_task(&self, Parameters(params): Parameters<ResumeTaskParams>) -> CallToolResult {
        self.post(
            &format!("/api/steward/tasks/{}/resume", params.task_id),
            json!({"restart": params.restart.unwrap_or(false)}),
        )
        .await
    }

    #[tool(
        name = "steward_list_archived",
        description = "List archived conversations and teams so an exact restore target can be resolved. This is read-only."
    )]
    async fn list_archived(&self, Parameters(params): Parameters<ListArchivedParams>) -> CallToolResult {
        self.get(
            "/api/sidebar",
            &[
                ("archived", "true".to_owned()),
                ("limit", params.limit.unwrap_or(100).clamp(1, 100).to_string()),
            ],
        )
        .await
    }

    #[tool(
        name = "steward_archive_conversation",
        description = "Soft-archive one exact conversation only after the user explicitly asked to archive that target. Resolve ambiguous names first. The steward itself and individual team workers are protected; a team leader archives its whole team."
    )]
    async fn archive_conversation(&self, Parameters(params): Parameters<ConversationIdParams>) -> CallToolResult {
        self.set_conversation_archived(&params.conversation_id, true).await
    }

    #[tool(
        name = "steward_restore_conversation",
        description = "Restore one exact archived conversation only after the user explicitly asked to restore that target. The steward itself and individual team workers are protected; restoring an archived team leader restores its whole team."
    )]
    async fn restore_conversation(&self, Parameters(params): Parameters<ConversationIdParams>) -> CallToolResult {
        self.set_conversation_archived(&params.conversation_id, false).await
    }
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for StewardStdioServer {}

impl StewardStdioServer {
    async fn get(&self, path: &str, query: &[(&str, String)]) -> CallToolResult {
        let mut request = self.client.get(self.url(path));
        for (key, value) in query {
            request = request.query(&[(*key, value)]);
        }
        self.send(request).await
    }

    async fn post(&self, path: &str, body: Value) -> CallToolResult {
        self.send(self.client.post(self.url(path)).json(&body)).await
    }

    async fn patch(&self, path: &str, body: Value) -> CallToolResult {
        self.send(self.client.patch(self.url(path)).json(&body)).await
    }

    async fn set_conversation_archived(&self, conversation_id: &str, archived: bool) -> CallToolResult {
        let conversation = match self
            .request_data(
                self.client
                    .get(self.url(&format!("/api/conversations/{conversation_id}"))),
            )
            .await
        {
            Ok(value) => value,
            Err(message) => return tool_error(message),
        };
        let target = match conversation_control_target(&self.env.conversation_id, &conversation) {
            Ok(target) => target,
            Err(message) => return tool_error(message),
        };
        let action = if archived { "archive" } else { "unarchive" };
        let (scope, target_id, conversation_id, conversation_name, team_id) = match target {
            ConversationControlTarget::Conversation {
                conversation_id,
                conversation_name,
            } => (
                "conversation",
                conversation_id.clone(),
                conversation_id,
                conversation_name,
                None,
            ),
            ConversationControlTarget::Team {
                conversation_id,
                conversation_name,
                team_id,
            } => (
                "team",
                team_id.clone(),
                conversation_id,
                conversation_name,
                Some(team_id),
            ),
        };
        let path = format!("/api/sidebar/{scope}/{target_id}/{action}");
        if let Err(message) = self.request_data(self.client.post(self.url(&path))).await {
            return tool_error(message);
        }
        let result = json!({
            "conversation_id": conversation_id,
            "conversation_name": conversation_name,
            "archived": archived,
            "affected_scope": scope,
            "team_id": team_id,
            "reversible": true,
        });
        CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
        )])
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.env.base_url.trim_end_matches('/'), path)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> CallToolResult {
        let data = match self.request_data(request).await {
            Ok(data) => data,
            Err(message) => return tool_error(message),
        };
        let text = serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string());
        CallToolResult::success(vec![Content::text(text)])
    }

    async fn request_data(&self, request: reqwest::RequestBuilder) -> Result<Value, String> {
        let headers = match self.env.headers() {
            Ok(headers) => headers,
            Err(message) => return Err(message),
        };
        let response = match request.headers(headers).send().await {
            Ok(response) => response,
            Err(error) => return Err(format!("AionUi steward API unavailable: {error}")),
        };
        let status = response.status();
        let value = match response.json::<Value>().await {
            Ok(value) => value,
            Err(error) => return Err(format!("invalid steward API response: {error}")),
        };
        if !status.is_success() || value.get("success").and_then(Value::as_bool) == Some(false) {
            let message = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("steward API request failed");
            return Err(format!("{message} (HTTP {status})"));
        }
        Ok(value.get("data").cloned().unwrap_or(value))
    }
}

fn conversation_control_target(
    steward_conversation_id: &str,
    conversation: &Value,
) -> Result<ConversationControlTarget, String> {
    let conversation_id = conversation
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "conversation response is missing id".to_owned())?;
    let conversation_name = conversation
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(conversation_id);
    let extra = conversation.get("extra").and_then(Value::as_object);
    let is_steward = extra.and_then(|value| value.get("steward")).and_then(Value::as_bool) == Some(true);
    if conversation_id == steward_conversation_id || is_steward {
        return Err("the steward conversation is protected and cannot archive or restore itself".to_owned());
    }
    let role = extra.and_then(|value| value.get("role")).and_then(Value::as_str);
    let team_id = extra
        .and_then(|value| value.get("teamId").or_else(|| value.get("team_id")))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    if role == Some("teammate") || (team_id.is_some() && role != Some("lead")) {
        return Err(
            "an individual team worker cannot be archived or restored; target the team's leader instead".to_owned(),
        );
    }
    if let Some(team_id) = team_id {
        return Ok(ConversationControlTarget::Team {
            conversation_id: conversation_id.to_owned(),
            conversation_name: conversation_name.to_owned(),
            team_id: team_id.to_owned(),
        });
    }
    Ok(ConversationControlTarget::Conversation {
        conversation_id: conversation_id.to_owned(),
        conversation_name: conversation_name.to_owned(),
    })
}

fn tool_error(message: String) -> CallToolResult {
    let mut result = CallToolResult::error(vec![Content::text(message.clone())]);
    result.structured_content = Some(json!({"code": "STEWARD_TOOL_FAILED", "message": message}));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_fit_common_provider_limit() {
        for name in [
            "steward_overview",
            "steward_search_tasks",
            "steward_list_tasks",
            "steward_inspect_task",
            "steward_create_task",
            "steward_update_task",
            "steward_list_assistants",
            "steward_create_session",
            "steward_bind_session",
            "steward_dispatch_task",
            "steward_ask_task",
            "steward_resume_task",
            "steward_list_archived",
            "steward_archive_conversation",
            "steward_restore_conversation",
        ] {
            assert!(format!("mcp__aionui-steward__{name}").len() <= 64, "{name}");
        }
    }

    #[test]
    fn conversation_control_protects_steward_and_team_workers() {
        for conversation in [
            serde_json::json!({"id": "steward-1", "name": "大管家", "extra": {}}),
            serde_json::json!({"id": "steward-2", "name": "旧管家", "extra": {"steward": true}}),
            serde_json::json!({
                "id": "worker-1",
                "name": "Worker",
                "extra": {"role": "teammate", "teamId": "team-1"}
            }),
            serde_json::json!({
                "id": "legacy-worker-1",
                "name": "Legacy Worker",
                "extra": {"teamId": "team-1"}
            }),
        ] {
            assert!(conversation_control_target("steward-1", &conversation).is_err());
        }
    }

    #[test]
    fn conversation_control_maps_team_leader_to_whole_team() {
        let conversation = serde_json::json!({
            "id": "leader-1",
            "name": "Leader",
            "extra": {"role": "lead", "teamId": "team-1"}
        });
        assert_eq!(
            conversation_control_target("steward-1", &conversation).unwrap(),
            ConversationControlTarget::Team {
                conversation_id: "leader-1".to_owned(),
                conversation_name: "Leader".to_owned(),
                team_id: "team-1".to_owned(),
            }
        );
    }

    #[test]
    fn conversation_control_keeps_independent_conversation_scope() {
        let conversation = serde_json::json!({"id": "conv-1", "name": "Standalone", "extra": {}});
        assert_eq!(
            conversation_control_target("steward-1", &conversation).unwrap(),
            ConversationControlTarget::Conversation {
                conversation_id: "conv-1".to_owned(),
                conversation_name: "Standalone".to_owned(),
            }
        );
    }
}
