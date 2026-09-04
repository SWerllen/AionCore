//! E2E tests for message delivery after ordinary conversations became latent
//! embedded teams.
//!
//! The embedded-team scheduler is now the owner of every visible user turn.
//! While its leader is active, a second message is accepted as a new queued
//! team run regardless of the backend's direct mid-turn capability.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::StatusCode;
use serde_json::json;
use tokio::sync::Mutex;
use tower::ServiceExt;

use aionui_ai_agent::{AgentInstance, IAgentTask, IMockAgent, WorkerTaskManagerImpl};
use aionui_app::{AppConfig, AppServices};
use common::{body_json, get_with_token, json_with_token, setup_and_login};

/// A mock agent whose event channel stays OPEN (the sender is held), so the
/// spawned turn relay keeps waiting for frames and the turn claim stays held —
/// modelling a long-running turn. `deliver_midturn` records the delivered
/// message instead of opening a turn.
struct MidturnMockAgent {
    conversation_id: String,
    supports_midturn: bool,
    tx: tokio::sync::broadcast::Sender<aionui_ai_agent::AgentStreamEvent>,
    delivered: Arc<Mutex<Vec<String>>>,
    send_called: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl IAgentTask for MidturnMockAgent {
    fn agent_type(&self) -> aionui_common::AgentType {
        aionui_common::AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        "/tmp/test"
    }
    fn status(&self) -> Option<aionui_common::ConversationStatus> {
        None
    }
    fn last_activity_at(&self) -> aionui_common::TimestampMs {
        aionui_common::now_ms()
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<aionui_ai_agent::AgentStreamEvent> {
        self.tx.subscribe()
    }
    fn supports_midturn_delivery(&self) -> bool {
        self.supports_midturn
    }
    async fn send_message(
        &self,
        _data: aionui_ai_agent::types::SendMessageData,
    ) -> Result<(), aionui_ai_agent::AgentSendError> {
        self.send_called.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn cancel(&self) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<aionui_common::AgentKillReason>) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl IMockAgent for MidturnMockAgent {
    async fn deliver_midturn(
        &self,
        data: aionui_ai_agent::types::SendMessageData,
    ) -> Result<(), aionui_ai_agent::AgentSendError> {
        self.delivered.lock().await.push(data.content);
        Ok(())
    }
}

struct MidturnRig {
    app: axum::Router,
    services: AppServices,
    delivered: Arc<Mutex<Vec<String>>>,
}

async fn build_midturn_app(supports_midturn: bool) -> MidturnRig {
    let db = aionui_db::init_database_memory().await.unwrap();
    let delivered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let delivered_for_factory = Arc::clone(&delivered);
    let factory: std::sync::Arc<
        dyn Fn(
                aionui_ai_agent::types::BuildTaskOptions,
            )
                -> futures_util::future::BoxFuture<'static, Result<AgentInstance, aionui_ai_agent::AgentError>>
            + Send
            + Sync,
    > = std::sync::Arc::new(move |opts| {
        let delivered = Arc::clone(&delivered_for_factory);
        Box::pin(async move {
            let (tx, _keep_open) = tokio::sync::broadcast::channel(16);
            Ok(AgentInstance::Mock(std::sync::Arc::new(MidturnMockAgent {
                conversation_id: opts.conversation_id().to_owned(),
                supports_midturn,
                tx,
                delivered,
                send_called: Arc::new(AtomicBool::new(false)),
            })))
        })
    });
    let wtm: std::sync::Arc<dyn aionui_ai_agent::IWorkerTaskManager> =
        std::sync::Arc::new(WorkerTaskManagerImpl::new(factory));
    let services = AppServices::from_config(db, &AppConfig::default())
        .await
        .unwrap()
        .with_worker_task_manager(wtm);
    let app = aionui_app::create_router(&services).await.expect("build router");
    MidturnRig {
        app,
        services,
        delivered,
    }
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str) -> String {
    let req = json_with_token(
        "POST",
        "/api/conversations",
        json!({ "type": "acp", "name": "midturn", "extra": { "backend": "gemini" } }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    json["data"]["id"].as_str().unwrap().to_owned()
}

async fn send_message(
    app: &mut axum::Router,
    conv_id: &str,
    content: &str,
    token: &str,
    csrf: &str,
) -> (StatusCode, serde_json::Value) {
    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({ "content": content }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Wait until the conversation's agent is registered with the task manager.
///
/// `send_message` returns 202 as soon as the turn is CLAIMED, but the agent
/// itself is built lazily inside the detached turn task. The mid-turn branch
/// needs `task_manager.get_task(...)` to be populated, so a second send issued
/// before registration completes legitimately falls through to the claim and
/// gets 409. Without this barrier the test races that spawn and fails
/// intermittently under parallel load.
async fn wait_for_agent_registered(services: &aionui_app::AppServices, conv_id: &str) {
    for _ in 0..200 {
        if services.worker_task_manager.get_task(conv_id).is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("agent for {conv_id} never registered with the task manager");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_team_send_queues_a_new_run_for_midturn_capable_backend() {
    let MidturnRig {
        mut app,
        services,
        delivered,
    } = build_midturn_app(true).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pw").await;
    let conv_id = create_conversation(&mut app, &token, &csrf).await;

    // Turn 1 opens and stays active: the mock's event channel never finishes.
    let (status1, body1) = send_message(&mut app, &conv_id, "first message", &token, &csrf).await;
    assert_eq!(status1, StatusCode::ACCEPTED, "first send scheduled a turn: {body1}");
    let turn1 = body1["data"]["turn_id"].as_str().unwrap().to_owned();
    // Prove the direct agent exists and advertises mid-turn delivery. The
    // embedded-team boundary must still own the second visible user message.
    wait_for_agent_registered(&services, &conv_id).await;

    // The second visible user message becomes a distinct queued team run.
    let (status2, body2) = send_message(&mut app, &conv_id, "midturn interjection", &token, &csrf).await;
    assert_eq!(
        status2,
        StatusCode::ACCEPTED,
        "embedded team send must be accepted, got {status2}: {body2}"
    );
    let turn2 = body2["data"]["turn_id"].as_str().unwrap();
    assert_ne!(turn2, turn1, "the queued message owns a distinct team run");
    assert!(
        body2["data"].get("delivered_midturn").is_none(),
        "the embedded scheduler must not report direct mid-turn delivery"
    );

    // It must not bypass the scheduler through the direct-agent interjection
    // path, even though that path is supported by this mock.
    let delivered = delivered.lock().await.clone();
    assert!(delivered.is_empty(), "direct mid-turn delivery must stay unused");

    // The scheduler-owned user message is persisted as handed off to the team,
    // and the list view shows it.
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let items = body["data"]["items"].as_array().unwrap();
    let row = items
        .iter()
        .find(|m| m["content"]["content"] == "midturn interjection")
        .expect("mid-turn user message persisted");
    assert_eq!(
        row["status"], "finish",
        "the team scheduler records the user message as handed off"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_team_send_also_queues_for_non_midturn_backend() {
    let MidturnRig { mut app, services, .. } = build_midturn_app(false).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pw").await;
    let conv_id = create_conversation(&mut app, &token, &csrf).await;

    let (status1, _) = send_message(&mut app, &conv_id, "first message", &token, &csrf).await;
    assert_eq!(status1, StatusCode::ACCEPTED);

    let (status2, body2) = send_message(&mut app, &conv_id, "second message", &token, &csrf).await;
    assert_eq!(
        status2,
        StatusCode::ACCEPTED,
        "embedded team scheduler accepts the queued run, got {status2}: {body2}"
    );
    assert!(body2["data"].get("delivered_midturn").is_none());
}
