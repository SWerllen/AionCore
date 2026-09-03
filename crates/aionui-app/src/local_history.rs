//! Local Codex / Qoder history adoption.
//!
//! The provider remains the execution/history authority.  AionUi only creates
//! an indexed conversation shell, binds it to the provider's native session id,
//! and groups shells that belong to the same repository under one project.

#![allow(clippy::disallowed_types)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use aionui_api_types::{ApiResponse, CreateConversationRequest};
use aionui_auth::CurrentUser;
use aionui_common::{AgentType, ConversationSource, generate_short_id, now_ms};
use aionui_conversation::ConversationService;
use aionui_db::models::MessageRow;
use aionui_db::{IAcpSessionRepository, IConversationRepository, SqlitePool};
use aionui_project::{AttachInput, ProjectService, canonical};
use axum::Router;
use axum::extract::{Extension, Json, State};
use axum::routing::{get, post};
use futures_util::stream::{self, StreamExt};
use git2::Repository;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const DEFAULT_MAX_SESSIONS_PER_PROVIDER: usize = 2_000;
const QODER_LIST_CONCURRENCY: usize = 12;
const SESSION_ADOPTION_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct LocalHistoryRouterState {
    pool: SqlitePool,
    conversation: ConversationService,
    conversation_repo: Arc<dyn IConversationRepository>,
    acp_session_repo: Arc<dyn IAcpSessionRepository>,
    project: ProjectService,
    sync_lock: Arc<tokio::sync::Mutex<()>>,
}

impl LocalHistoryRouterState {
    pub fn new(
        pool: SqlitePool,
        conversation: ConversationService,
        conversation_repo: Arc<dyn IConversationRepository>,
        acp_session_repo: Arc<dyn IAcpSessionRepository>,
        project: ProjectService,
    ) -> Self {
        Self {
            pool,
            conversation,
            conversation_repo,
            acp_session_repo,
            project,
            sync_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

pub fn local_history_routes(state: LocalHistoryRouterState) -> Router {
    Router::new()
        .route("/api/local-history/status", get(status))
        .route("/api/local-history/sync", post(sync))
        .with_state(state)
}

#[derive(Debug, Default, Deserialize)]
pub struct SyncRequest {
    #[serde(default)]
    providers: Vec<String>,
    max_sessions_per_provider: Option<usize>,
}

#[derive(Debug, Default, Serialize)]
pub struct ProviderSyncResult {
    discovered: usize,
    imported: usize,
    linked_existing: usize,
    already_synced: usize,
    tombstoned: usize,
    skipped: usize,
    errors: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct SyncResponse {
    codex: ProviderSyncResult,
    qoder: ProviderSyncResult,
    projects_grouped: usize,
    synced_at: i64,
}

#[derive(Debug, Serialize)]
struct SyncStatusResponse {
    codex: i64,
    qoder: i64,
    last_synced_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct NativeSession {
    provider: &'static str,
    native_id: String,
    title: String,
    preview: Option<String>,
    cwd: PathBuf,
    source_path: Option<PathBuf>,
    updated_at: i64,
    created_at: i64,
    archived: bool,
    project_key: String,
    project_root: PathBuf,
}

#[derive(Debug, sqlx::FromRow)]
struct BindingRow {
    provider: String,
    native_session_id: String,
    conversation_id: String,
    conversation_exists: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct AnchorRow {
    session_id: String,
    conversation_id: String,
    extra: String,
}

async fn status(
    State(state): State<LocalHistoryRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<SyncStatusResponse>>, aionui_common::ApiError> {
    let codex = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM native_session_bindings WHERE user_id = ? AND provider = 'codex'",
    )
    .bind(&user.id)
    .fetch_one(&state.pool)
    .await
    .map_err(internal)?;
    let qoder = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM native_session_bindings WHERE user_id = ? AND provider = 'qoder'",
    )
    .bind(&user.id)
    .fetch_one(&state.pool)
    .await
    .map_err(internal)?;
    let last_synced_at =
        sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(synced_at) FROM native_session_bindings WHERE user_id = ?")
            .bind(&user.id)
            .fetch_one(&state.pool)
            .await
            .map_err(internal)?;
    Ok(Json(ApiResponse::ok(SyncStatusResponse {
        codex,
        qoder,
        last_synced_at,
    })))
}

async fn sync(
    State(state): State<LocalHistoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(request): Json<SyncRequest>,
) -> Result<Json<ApiResponse<SyncResponse>>, aionui_common::ApiError> {
    let _guard = state.sync_lock.lock().await;
    let providers: HashSet<String> = if request.providers.is_empty() {
        ["codex".to_owned(), "qoder".to_owned()].into_iter().collect()
    } else {
        request
            .providers
            .into_iter()
            .map(|provider| provider.to_ascii_lowercase())
            .filter(|provider| provider == "codex" || provider == "qoder")
            .collect()
    };
    let limit = request
        .max_sessions_per_provider
        .unwrap_or(DEFAULT_MAX_SESSIONS_PER_PROVIDER)
        .clamp(1, 10_000);

    let (codex_scan, qoder_scan) = tokio::join!(
        async {
            if providers.contains("codex") {
                scan_codex(limit).await
            } else {
                Ok(Vec::new())
            }
        },
        async {
            if providers.contains("qoder") {
                scan_qoder(limit).await
            } else {
                Ok(Vec::new())
            }
        }
    );

    let mut response = SyncResponse::default();
    let mut sessions = Vec::new();
    match codex_scan {
        Ok(mut found) => {
            response.codex.discovered = found.len();
            sessions.append(&mut found);
        }
        Err(error) => response.codex.errors.push(error),
    }
    match qoder_scan {
        Ok(mut found) => {
            response.qoder.discovered = found.len();
            sessions.append(&mut found);
        }
        Err(error) => response.qoder.errors.push(error),
    }

    merge_orphaned_worktrees(&mut sessions);
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let bindings = load_bindings(&state.pool, &user.id).await.map_err(internal)?;
    let anchors = load_native_anchors(&state.pool, &user.id).await.map_err(internal)?;
    let mut binding_map: HashMap<(String, String), BindingRow> = bindings
        .into_iter()
        .map(|row| ((row.provider.clone(), row.native_session_id.clone()), row))
        .collect();
    let anchor_map: HashMap<(String, String), String> = anchors
        .into_iter()
        .filter_map(|row| {
            let backend = serde_json::from_str::<Value>(&row.extra)
                .ok()
                .and_then(|value| value.get("backend").and_then(Value::as_str).map(str::to_owned))?;
            Some(((backend, row.session_id), row.conversation_id))
        })
        .collect();

    let mut grouped_projects = HashSet::new();
    for session in sessions {
        let result = if session.provider == "codex" {
            &mut response.codex
        } else {
            &mut response.qoder
        };
        let key = (session.provider.to_owned(), session.native_id.clone());

        if let Some(binding) = binding_map.get(&key) {
            update_binding_metadata(&state.pool, &user.id, &session)
                .await
                .map_err(internal)?;
            if !binding.conversation_exists {
                // Deletion is deliberate.  Keep the durable bridge row as a
                // tombstone so rescans never resurrect the removed task.
                result.tombstoned += 1;
                continue;
            }
            if tokio::time::timeout(
                SESSION_ADOPTION_TIMEOUT,
                bind_conversation_project(&state, &user.id, &binding.conversation_id, &session),
            )
            .await
            .is_ok_and(|result| result.is_ok())
            {
                grouped_projects.insert(session.project_key.clone());
            }
            result.already_synced += 1;
            continue;
        }

        if let Some(conversation_id) = anchor_map.get(&key) {
            insert_binding(&state.pool, &user.id, conversation_id, &session, false)
                .await
                .map_err(internal)?;
            let _ = bind_conversation_project(&state, &user.id, conversation_id, &session).await;
            grouped_projects.insert(session.project_key.clone());
            result.linked_existing += 1;
            continue;
        }

        match tokio::time::timeout(SESSION_ADOPTION_TIMEOUT, import_session(&state, &user.id, &session)).await {
            Ok(Ok(conversation_id)) => {
                insert_binding(&state.pool, &user.id, &conversation_id, &session, true)
                    .await
                    .map_err(internal)?;
                binding_map.insert(
                    key,
                    BindingRow {
                        provider: session.provider.to_owned(),
                        native_session_id: session.native_id.clone(),
                        conversation_id,
                        conversation_exists: true,
                    },
                );
                grouped_projects.insert(session.project_key.clone());
                result.imported += 1;
            }
            Ok(Err(error)) => {
                tracing::warn!(provider = session.provider, native_session_id = %session.native_id, error = %error, "native history import skipped");
                result.skipped += 1;
                if result.errors.len() < 10 {
                    result.errors.push(format!("{}: {error}", short_id(&session.native_id)));
                }
            }
            Err(_) => {
                tracing::warn!(provider = session.provider, native_session_id = %session.native_id, "native history import timed out");
                result.skipped += 1;
                if result.errors.len() < 10 {
                    result
                        .errors
                        .push(format!("{}: adoption timed out", short_id(&session.native_id)));
                }
            }
        }
    }

    response.projects_grouped = grouped_projects.len();
    response.synced_at = now_ms();
    Ok(Json(ApiResponse::ok(response)))
}

/// A removed or partially cleaned native worktree can leave the workspace
/// directory behind without its `.git` pointer.  In that case git2 cannot
/// identify the repository and the task would otherwise appear as a second,
/// same-named project.  Reuse an unambiguous repository identity discovered
/// from another local session with the same checkout name.
fn merge_orphaned_worktrees(sessions: &mut [NativeSession]) {
    let mut repositories: HashMap<String, Option<(String, PathBuf)>> = HashMap::new();
    for session in sessions
        .iter()
        .filter(|session| session.project_key.starts_with("git:"))
    {
        let Some(name) = session.project_root.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let identity = (session.project_key.clone(), session.project_root.clone());
        repositories
            .entry(name.to_owned())
            .and_modify(|current| {
                if current.as_ref() != Some(&identity) {
                    *current = None;
                }
            })
            .or_insert(Some(identity));
    }

    for session in sessions
        .iter_mut()
        .filter(|session| session.project_key.starts_with("path:") && is_native_worktree_path(&session.cwd))
    {
        let Some(name) = session.cwd.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if let Some(Some((project_key, project_root))) = repositories.get(name) {
            session.project_key.clone_from(project_key);
            session.project_root.clone_from(project_root);
        }
    }
}

fn is_native_worktree_path(path: &Path) -> bool {
    let value = path.to_string_lossy();
    value.contains("/.codex/worktrees/") || value.contains("/.qoder/worktrees/")
}

async fn import_session(
    state: &LocalHistoryRouterState,
    user_id: &str,
    session: &NativeSession,
) -> Result<String, String> {
    if !session.cwd.is_dir() {
        return Err(format!("workspace is unavailable: {}", session.cwd.display()));
    }
    let extra = json!({
        "backend": session.provider,
        "agent_source": "builtin",
        "workspace": session.cwd,
        "native_history": {
            "provider": session.provider,
            "session_id": session.native_id,
            "source_path": session.source_path,
            "project_root": session.project_root,
            "imported": true
        }
    });
    let created = state
        .conversation
        .create(
            user_id,
            CreateConversationRequest {
                r#type: Some(AgentType::Acp),
                name: Some(session.title.clone()),
                model: None,
                assistant: None,
                source: Some(ConversationSource::Aionui),
                channel_chat_id: None,
                extra,
            },
        )
        .await
        .map_err(|error| error.to_string())?;

    state
        .acp_session_repo
        .update_session_id_for_user(user_id, &created.id, &session.native_id)
        .await
        .map_err(|error| error.to_string())?;

    bind_conversation_project(state, user_id, &created.id, session).await?;

    sqlx::query(
        "UPDATE conversations SET created_at = ?, updated_at = ?, name_source = 'agent', archived_at = ? \
         WHERE user_id = ? AND id = ?",
    )
    .bind(session.created_at)
    .bind(session.updated_at)
    .bind(session.archived.then_some(session.updated_at))
    .bind(user_id)
    .bind(&created.id)
    .execute(&state.pool)
    .await
    .map_err(|error| error.to_string())?;

    let preview = session.preview.as_deref().unwrap_or(&session.title);
    let marker = MessageRow {
        id: generate_short_id(),
        conversation_id: created.id.clone(),
        msg_id: None,
        r#type: "tips".to_owned(),
        content: json!({
            "content": format!(
                "已从 {} 同步原生任务。历史仍由 {} 保存；下一条消息会直接续接原会话。\n\n最近请求：{}",
                provider_name(session.provider),
                provider_name(session.provider),
                truncate(preview, 500)
            ),
            "type": "info",
            "code": "NATIVE_HISTORY_IMPORTED",
            "params": {
                "provider": session.provider,
                "native_session_id": session.native_id
            }
        })
        .to_string(),
        position: Some("center".to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: session.updated_at,
        backend_turn_id: None,
    };
    state
        .conversation_repo
        .insert_message(user_id, &marker)
        .await
        .map_err(|error| error.to_string())?;
    // insert_message bumps updated_at to now; restore the provider's activity
    // timestamp so project/task ordering matches the native clients.
    sqlx::query("UPDATE conversations SET updated_at = ? WHERE user_id = ? AND id = ?")
        .bind(session.updated_at)
        .bind(user_id)
        .bind(&created.id)
        .execute(&state.pool)
        .await
        .map_err(|error| error.to_string())?;
    Ok(created.id)
}

async fn bind_conversation_project(
    state: &LocalHistoryRouterState,
    user_id: &str,
    conversation_id: &str,
    session: &NativeSession,
) -> Result<(), String> {
    let project_uri = canonical::to_file_uri(&session.project_root).map_err(|error| error.to_string())?;
    let group = state
        .project
        .resolve_existing(user_id, project_uri)
        .await
        .map_err(|error| error.to_string())?;

    let folder_id = if session.cwd == session.project_root {
        group.folder.folder_id.clone()
    } else {
        let uri = canonical::to_file_uri(&session.cwd).map_err(|error| error.to_string())?;
        match state
            .project
            .attach_folder(
                user_id,
                AttachInput {
                    project_id: group.project.project_id.clone(),
                    uri,
                    display_name: None,
                },
            )
            .await
        {
            Ok(entry) => entry.folder_id,
            Err(error) if error.code() == "project_explorer_duplicate" => group.folder.folder_id.clone(),
            Err(error) => return Err(error.to_string()),
        }
    };

    sqlx::query("UPDATE conversations SET project_id = ?, folder_id = ? WHERE user_id = ? AND id = ?")
        .bind(&group.project.project_id)
        .bind(folder_id)
        .bind(user_id)
        .bind(conversation_id)
        .execute(&state.pool)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn load_bindings(pool: &SqlitePool, user_id: &str) -> Result<Vec<BindingRow>, sqlx::Error> {
    sqlx::query_as::<_, BindingRow>(
        "SELECT b.provider, b.native_session_id, b.conversation_id, \
                EXISTS(SELECT 1 FROM conversations c WHERE c.user_id = b.user_id AND c.id = b.conversation_id) AS conversation_exists \
         FROM native_session_bindings b WHERE b.user_id = ?",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

async fn load_native_anchors(pool: &SqlitePool, user_id: &str) -> Result<Vec<AnchorRow>, sqlx::Error> {
    sqlx::query_as::<_, AnchorRow>(
        "SELECT a.session_id, c.id AS conversation_id, c.extra \
         FROM acp_session a JOIN conversations c ON c.id = a.conversation_id \
         WHERE c.user_id = ? AND a.session_id IS NOT NULL AND TRIM(a.session_id) <> ''",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

async fn insert_binding(
    pool: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
    session: &NativeSession,
    imported: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO native_session_bindings \
         (user_id, provider, native_session_id, conversation_id, source_path, cwd, project_key, title, source_updated_at, archived, imported, synced_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(session.provider)
    .bind(&session.native_id)
    .bind(conversation_id)
    .bind(session.source_path.as_ref().map(|path| path.to_string_lossy().into_owned()))
    .bind(session.cwd.to_string_lossy().into_owned())
    .bind(&session.project_key)
    .bind(&session.title)
    .bind(session.updated_at)
    .bind(session.archived)
    .bind(imported)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

async fn update_binding_metadata(pool: &SqlitePool, user_id: &str, session: &NativeSession) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE native_session_bindings SET source_path = ?, cwd = ?, project_key = ?, title = ?, \
         source_updated_at = ?, archived = ?, synced_at = ? \
         WHERE user_id = ? AND provider = ? AND native_session_id = ?",
    )
    .bind(
        session
            .source_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
    )
    .bind(session.cwd.to_string_lossy().into_owned())
    .bind(&session.project_key)
    .bind(&session.title)
    .bind(session.updated_at)
    .bind(session.archived)
    .bind(now_ms())
    .bind(user_id)
    .bind(session.provider)
    .bind(&session.native_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexThread {
    id: String,
    name: Option<String>,
    preview: Option<String>,
    cwd: String,
    path: Option<String>,
    created_at: i64,
    updated_at: i64,
    #[serde(default)]
    ephemeral: bool,
    parent_thread_id: Option<String>,
}

async fn scan_codex(limit: usize) -> Result<Vec<NativeSession>, String> {
    let (app_server, files) = tokio::join!(scan_codex_app_server(limit), scan_codex_files(limit));
    let mut merged = HashMap::new();
    let mut errors = Vec::new();
    match files {
        Ok(sessions) => {
            for session in sessions {
                merged.insert(session.native_id.clone(), session);
            }
        }
        Err(error) => errors.push(error),
    }
    // App-server metadata is richer (user-visible title/preview and authoritative
    // archive state), so it wins when the same rollout also exists on disk.
    match app_server {
        Ok(sessions) => {
            for session in sessions {
                merged.insert(session.native_id.clone(), session);
            }
        }
        Err(error) => errors.push(error),
    }
    if merged.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let mut sessions: Vec<_> = merged.into_values().collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    sessions.truncate(limit);
    Ok(sessions)
}

async fn scan_codex_app_server(limit: usize) -> Result<Vec<NativeSession>, String> {
    let mut child = Command::new("codex")
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("cannot start codex app-server: {error}"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| "codex stdin unavailable".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "codex stdout unavailable".to_owned())?;
    let mut lines = BufReader::new(stdout).lines();

    write_rpc(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "AionUi local history", "version": "1"}, "capabilities": {"experimentalApi": true}}
        }),
    )
    .await?;
    let initialized = read_rpc_result(&mut lines, 1).await?;
    if initialized.get("error").is_some() {
        return Err(format!("codex initialize failed: {}", initialized["error"]));
    }
    write_rpc(&mut stdin, json!({"jsonrpc":"2.0", "method":"initialized"})).await?;

    let mut found = Vec::new();
    for archived in [false, true] {
        let mut cursor: Option<String> = None;
        loop {
            let request_id = 10 + found.len() as i64;
            let mut params = json!({"limit": 100, "archived": archived});
            if let Some(value) = cursor.as_ref() {
                params["cursor"] = Value::String(value.clone());
            }
            write_rpc(
                &mut stdin,
                json!({"jsonrpc":"2.0", "id": request_id, "method":"thread/list", "params": params}),
            )
            .await?;
            let response = read_rpc_result(&mut lines, request_id).await?;
            if let Some(error) = response.get("error") {
                return Err(format!("codex thread/list failed: {error}"));
            }
            let result = response.get("result").cloned().unwrap_or(Value::Null);
            let page: Vec<CodexThread> = serde_json::from_value(result.get("data").cloned().unwrap_or(json!([])))
                .map_err(|error| format!("invalid codex thread/list response: {error}"))?;
            for item in page {
                if item.ephemeral || item.parent_thread_id.is_some() || item.cwd.trim().is_empty() {
                    continue;
                }
                let cwd = PathBuf::from(&item.cwd);
                if !cwd.is_dir() {
                    continue;
                }
                let (project_key, project_root) = project_identity(&cwd);
                let title = item
                    .name
                    .filter(|name| !name.trim().is_empty())
                    .or_else(|| item.preview.as_ref().map(|preview| first_nonempty_line(preview)))
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("Codex {}", short_id(&item.id)));
                found.push(NativeSession {
                    provider: "codex",
                    native_id: item.id,
                    title: truncate(&title, 160),
                    preview: item.preview,
                    cwd,
                    source_path: item.path.map(PathBuf::from),
                    updated_at: item.updated_at.saturating_mul(1_000),
                    created_at: item.created_at.saturating_mul(1_000),
                    archived,
                    project_key,
                    project_root,
                });
            }
            if found.len() >= limit {
                break;
            }
            cursor = result.get("nextCursor").and_then(Value::as_str).map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        if found.len() >= limit {
            break;
        }
    }
    let _ = child.kill().await;
    found.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    found.truncate(limit);
    Ok(found)
}

async fn scan_codex_files(limit: usize) -> Result<Vec<NativeSession>, String> {
    tokio::task::spawn_blocking(move || scan_codex_files_blocking(limit))
        .await
        .map_err(|error| format!("codex filesystem scan failed: {error}"))?
}

fn scan_codex_files_blocking(limit: usize) -> Result<Vec<NativeSession>, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory unavailable".to_owned())?;
    let roots = [
        (home.join(".codex/sessions"), false),
        (home.join(".codex/archived_sessions"), true),
    ];
    let mut files = Vec::new();
    for (root, archived) in roots {
        collect_codex_jsonl_files(&root, archived, &mut files);
    }
    files.sort_by_key(|(path, _)| {
        std::cmp::Reverse(
            path.metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or_default(),
        )
    });

    let mut sessions = Vec::new();
    for (path, archived) in files {
        if sessions.len() >= limit {
            break;
        }
        if let Some(session) = read_codex_file_metadata(&path, archived) {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

fn collect_codex_jsonl_files(root: &Path, archived: bool, files: &mut Vec<(PathBuf, bool)>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_codex_jsonl_files(&entry.path(), archived, files);
        } else if file_type.is_file() && entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl") {
            files.push((entry.path(), archived));
        }
    }
}

fn read_codex_file_metadata(path: &Path, archived: bool) -> Option<NativeSession> {
    use std::io::BufRead as _;

    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let updated_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default();
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let envelope: Value = serde_json::from_str(&line).ok()?;
    if envelope.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = envelope.get("payload")?;
    let native_id = payload
        .get("id")
        .or_else(|| payload.get("session_id"))
        .and_then(Value::as_str)?
        .to_owned();
    let cwd = PathBuf::from(payload.get("cwd").and_then(Value::as_str)?);
    if !cwd.is_dir() {
        return None;
    }

    let preview = read_codex_first_user_prompt(&mut reader);
    let fallback_title = path
        .file_stem()
        .and_then(|value| value.to_str())
        .and_then(|value| value.strip_prefix("rollout-"))
        .map(|value| format!("Codex {}", value.get(..16).unwrap_or(value).replace('T', " ")))
        .unwrap_or_else(|| format!("Codex {}", short_id(&native_id)));
    let title = preview
        .as_deref()
        .map(title_from_codex_prompt)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_title);
    let (project_key, project_root) = project_identity(&cwd);
    Some(NativeSession {
        provider: "codex",
        native_id,
        title: truncate(&title, 160),
        preview,
        cwd,
        source_path: Some(path.to_path_buf()),
        updated_at,
        created_at: updated_at,
        archived,
        project_key,
        project_root,
    })
}

fn read_codex_first_user_prompt(reader: &mut impl std::io::BufRead) -> Option<String> {
    let mut line = String::new();
    let mut scanned = 0usize;
    while scanned < 1024 * 1024 {
        line.clear();
        let bytes = reader.read_line(&mut line).ok()?;
        if bytes == 0 {
            break;
        }
        scanned = scanned.saturating_add(bytes);
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        if payload.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = payload
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

fn title_from_codex_prompt(prompt: &str) -> String {
    let candidate = prompt
        .rsplit_once("## My request:")
        .map(|(_, value)| value)
        .or_else(|| prompt.rsplit_once("</environment_context>").map(|(_, value)| value))
        .unwrap_or(prompt);
    candidate
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with('<')
                && !line.starts_with("# Files mentioned")
                && !line.starts_with("<image")
        })
        .map(str::to_owned)
        .unwrap_or_default()
}

async fn write_rpc(stdin: &mut tokio::process::ChildStdin, value: Value) -> Result<(), String> {
    stdin
        .write_all(format!("{}\n", value).as_bytes())
        .await
        .map_err(|error| error.to_string())
}

async fn read_rpc_result(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: i64,
) -> Result<Value, String> {
    loop {
        let line = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .map_err(|_| format!("codex app-server timed out waiting for request {id}"))?
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "codex app-server closed unexpectedly".to_owned())?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return Ok(value);
        }
    }
}

#[derive(Debug, Clone)]
struct QoderProjectCandidate {
    workspace: PathBuf,
    sessions: Vec<(String, PathBuf, i64)>,
}

async fn scan_qoder(limit: usize) -> Result<Vec<NativeSession>, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory unavailable".to_owned())?;
    let root = home.join(".qoder/projects");
    let entries = std::fs::read_dir(&root).map_err(|error| format!("cannot scan {}: {error}", root.display()))?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() {
            continue;
        }
        let encoded = entry.file_name().to_string_lossy().into_owned();
        let Some(workspace) = resolve_qoder_workspace(&encoded) else {
            continue;
        };
        if !workspace.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        let mut sessions = Vec::new();
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()).map(str::to_owned) else {
                continue;
            };
            if !looks_like_uuid(&id) {
                continue;
            }
            let updated_at = file
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
                .unwrap_or_default();
            sessions.push((id, path, updated_at));
        }
        if !sessions.is_empty() {
            candidates.push(QoderProjectCandidate { workspace, sessions });
        }
    }

    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.sessions.iter().map(|s| s.2).max().unwrap_or(0)));
    let mut selected = Vec::new();
    let mut selected_count = 0;
    for mut candidate in candidates {
        candidate.sessions.sort_by_key(|session| std::cmp::Reverse(session.2));
        if selected_count >= limit {
            break;
        }
        candidate.sessions.truncate(limit - selected_count);
        selected_count += candidate.sessions.len();
        selected.push(candidate);
    }

    let results: Vec<Vec<NativeSession>> = stream::iter(
        selected
            .into_iter()
            .map(|candidate| async move { scan_qoder_project(candidate).await }),
    )
    .buffer_unordered(QODER_LIST_CONCURRENCY)
    .collect()
    .await;
    let mut found: Vec<NativeSession> = results.into_iter().flatten().collect();
    found.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    found.truncate(limit);
    Ok(found)
}

async fn scan_qoder_project(candidate: QoderProjectCandidate) -> Vec<NativeSession> {
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new("qodercli")
            .arg("-w")
            .arg(&candidate.workspace)
            .arg("--list-sessions")
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let titles = match output {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            parse_qoder_titles(&text)
        }
        _ => HashMap::new(),
    };
    let (project_key, project_root) = project_identity(&candidate.workspace);
    candidate
        .sessions
        .into_iter()
        .map(|(id, path, updated_at)| {
            let title = titles
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("Qoder {}", short_id(&id)));
            NativeSession {
                provider: "qoder",
                native_id: id,
                preview: Some(title.clone()),
                title: truncate(&title, 160),
                cwd: candidate.workspace.clone(),
                source_path: Some(path),
                updated_at,
                created_at: updated_at,
                archived: false,
                project_key: project_key.clone(),
                project_root: project_root.clone(),
            }
        })
        .collect()
}

fn parse_qoder_titles(output: &str) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let Some(open) = trimmed.rfind(" [") else { continue };
        if !trimmed.ends_with(']') {
            continue;
        }
        let id = &trimmed[open + 2..trimmed.len() - 1];
        if !looks_like_uuid(id) {
            continue;
        }
        let before_id = &trimmed[..open];
        let title_end = before_id.rfind(" (").unwrap_or(before_id.len());
        let without_age = before_id[..title_end].trim();
        let title = without_age
            .split_once(". ")
            .map(|(_, title)| title)
            .unwrap_or(without_age)
            .trim();
        if !title.is_empty() {
            titles.insert(id.to_owned(), title.to_owned());
        }
    }
    titles
}

/// Resolve Qoder's project key, which is the absolute cwd with `/` replaced by
/// `-`, without guessing where hyphens belong.  At each filesystem level only
/// real child directory names that match the remaining encoded prefix are
/// considered, so names containing hyphens remain unambiguous.
fn resolve_qoder_workspace(encoded: &str) -> Option<PathBuf> {
    fn descend(current: &Path, remaining: &str, depth: usize) -> Option<PathBuf> {
        if remaining.is_empty() {
            return current.is_dir().then(|| current.to_path_buf());
        }
        if depth > 24 || !remaining.starts_with('-') {
            return None;
        }
        let entries = std::fs::read_dir(current).ok()?;
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let segment = format!("-{}", entry.file_name().to_string_lossy());
            if let Some(rest) = remaining.strip_prefix(&segment)
                && (rest.is_empty() || rest.starts_with('-'))
                && let Some(found) = descend(&entry.path(), rest, depth + 1)
            {
                return Some(found);
            }
        }
        None
    }
    descend(Path::new("/"), encoded, 0).map(|path| dunce::canonicalize(&path).unwrap_or(path))
}

fn project_identity(cwd: &Path) -> (String, PathBuf) {
    let cwd = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let Ok(repo) = Repository::discover(&cwd) else {
        return (format!("path:{}", cwd.to_string_lossy()), cwd);
    };
    let workdir = repo.workdir().map(Path::to_path_buf).unwrap_or_else(|| cwd.clone());
    let common_dir = repo.commondir().to_path_buf();
    let main_root = if common_dir.file_name().and_then(|name| name.to_str()) == Some(".git") {
        common_dir
            .parent()
            .filter(|parent| parent.is_dir())
            .map(Path::to_path_buf)
    } else {
        None
    }
    .unwrap_or(workdir);
    let main_root = dunce::canonicalize(&main_root).unwrap_or(main_root);
    let origin = repo
        .find_remote("origin")
        .ok()
        .and_then(|remote| remote.url().ok().map(str::to_owned))
        .map(|url| url.trim_end_matches('/').trim_end_matches(".git").to_owned())
        .filter(|url| !url.is_empty());
    let key = origin
        .map(|origin| format!("git:{origin}"))
        .unwrap_or_else(|| format!("gitdir:{}", common_dir.to_string_lossy()));
    (key, main_root)
}

fn looks_like_uuid(value: &str) -> bool {
    value.len() == 36
        && value.as_bytes().iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn provider_name(provider: &str) -> &'static str {
    if provider == "qoder" { "Qoder" } else { "Codex" }
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn first_nonempty_line(value: &str) -> String {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn internal(error: impl std::fmt::Display) -> aionui_common::ApiError {
    aionui_common::ApiError::Internal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qoder_session_rows() {
        let titles = parse_qoder_titles(
            "Available sessions for this project (2):\n  1. Fix project grouping (2 days ago) [3f63c98d-2888-41d7-9e21-9facf2c5ebd3]\n  2. Empty conversation (1 hour ago) [cd3944d8-1f7e-496a-9b9c-6b1220b66143]",
        );
        assert_eq!(
            titles.get("3f63c98d-2888-41d7-9e21-9facf2c5ebd3").map(String::as_str),
            Some("Fix project grouping")
        );
    }

    #[test]
    fn uuid_shape_is_strict() {
        assert!(looks_like_uuid("3f63c98d-2888-41d7-9e21-9facf2c5ebd3"));
        assert!(!looks_like_uuid("not-a-session"));
    }

    #[test]
    fn codex_title_ignores_ambient_context() {
        let prompt = "<environment_context>\ninternal\n</environment_context>\n同步本地任务\n并按项目分组";
        assert_eq!(title_from_codex_prompt(prompt), "同步本地任务");
        let explicit = "noise\n## My request:\n修复同步按钮\n";
        assert_eq!(title_from_codex_prompt(explicit), "修复同步按钮");
    }

    #[test]
    fn reads_codex_rollout_metadata_without_loading_history() {
        use std::io::Write as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-2026-09-03T10-20-30-session.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "type": "session_meta",
                "payload": {
                    "id": "3f63c98d-2888-41d7-9e21-9facf2c5ebd3",
                    "cwd": temp.path()
                }
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "同步旧任务并按项目分组"}]
                }
            })
        )
        .unwrap();
        drop(file);

        let session = read_codex_file_metadata(&path, false).expect("valid rollout metadata");
        assert_eq!(session.provider, "codex");
        assert_eq!(session.title, "同步旧任务并按项目分组");
        assert_eq!(session.cwd, temp.path());
        assert_eq!(session.source_path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn orphaned_native_worktree_joins_the_unambiguous_repository() {
        let make_session = |cwd: &str, project_key: &str, project_root: &str| NativeSession {
            provider: "codex",
            native_id: cwd.to_owned(),
            title: String::new(),
            preview: None,
            cwd: PathBuf::from(cwd),
            source_path: None,
            updated_at: 0,
            created_at: 0,
            archived: false,
            project_key: project_key.to_owned(),
            project_root: PathBuf::from(project_root),
        };
        let mut sessions = vec![
            make_session(
                "/Users/test/codes/example",
                "git:git@example.com:team/example",
                "/Users/test/codes/example",
            ),
            make_session(
                "/Users/test/.codex/worktrees/1234/example",
                "path:/Users/test/.codex/worktrees/1234/example",
                "/Users/test/.codex/worktrees/1234/example",
            ),
        ];

        merge_orphaned_worktrees(&mut sessions);

        assert_eq!(sessions[1].project_key, "git:git@example.com:team/example");
        assert_eq!(sessions[1].project_root, PathBuf::from("/Users/test/codes/example"));
        assert_eq!(
            sessions[1].cwd,
            PathBuf::from("/Users/test/.codex/worktrees/1234/example")
        );
    }
}
