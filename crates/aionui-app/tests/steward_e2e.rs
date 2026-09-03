mod common;

use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, build_app_with_mock_agents, get_with_token, json_with_token, setup_and_login};

#[tokio::test]
async fn terminal_event_persists_one_proactive_steward_report() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let steward_conversation_id = "steward-terminal-test".to_owned();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES (?, 'system_default_user', '大管家', 'acp', '{\"steward\":true}', 'finished', 1, 1)",
    )
    .bind(&steward_conversation_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO steward_profiles (user_id, conversation_id, created_at, updated_at)
         VALUES ('system_default_user', ?, 1, 1)",
    )
    .bind(&steward_conversation_id)
    .execute(services.database.pool())
    .await
    .unwrap();

    let conversation = json_with_token(
        "POST",
        "/api/conversations",
        json!({"type": "acp", "name": "单智能体写作", "extra": {}}),
        &token,
        &csrf,
    );
    let conversation = body_json(app.clone().oneshot(conversation).await.unwrap()).await;
    let conversation_id = conversation["data"]["id"].as_str().unwrap().to_owned();
    let task = json_with_token(
        "POST",
        "/api/steward/tasks",
        json!({
            "title": "小说创作测试",
            "objective": "完成一章",
            "conversation_id": conversation_id,
        }),
        &token,
        &csrf,
    );
    let task = body_json(app.clone().oneshot(task).await.unwrap()).await;
    let task_id = task["data"]["id"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE steward_tasks SET execution_state = 'running', progress_summary = '章节已落盘', next_action = '等待审阅' WHERE id = ?")
        .bind(&task_id)
        .execute(services.database.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO steward_task_events (id, task_id, source, event_type, payload, created_at)
         VALUES ('dispatch-terminal-test', ?, 'steward', 'task_dispatched', '{\"turn_id\":\"turn-terminal-test\"}', 10)",
    )
    .bind(&task_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages
         (id, conversation_id, msg_id, type, content, position, status, hidden, created_at, backend_turn_id)
         VALUES ('leader-final-test', ?, 'leader-final-test', 'text',
                 '{\"content\":\"本轮真实结果：已修改 7 处指代问题。\"}',
                 'left', 'finish', 0, 11, 'native-turn-test')",
    )
    .bind(&conversation_id)
    .execute(services.database.pool())
    .await
    .unwrap();

    let terminal = WebSocketMessage::new(
        "team.runCompleted",
        json!({
            "user_id": "system_default_user",
            "team_run_id": "turn-terminal-test",
            "status": "completed",
        }),
    );
    services.event_bus.broadcast(terminal.clone());
    services.event_bus.broadcast(terminal);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages
             WHERE conversation_id = ? AND backend_turn_id = 'steward-report:turn-terminal-test'",
        )
        .bind(&steward_conversation_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
        if count == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "completion report was not delivered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let report: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT inbox_delivered_at, im_delivered_at FROM steward_report_outbox
         WHERE task_id = ? AND run_id = 'turn-terminal-test'",
    )
    .bind(&task_id)
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    assert!(report.0.is_some());
    assert!(
        report.1.is_some(),
        "no IM binding is still a completed delivery decision"
    );
    let state: String = sqlx::query_scalar("SELECT execution_state FROM steward_tasks WHERE id = ?")
        .bind(&task_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(state, "idle");
    let content: String = sqlx::query_scalar(
        "SELECT content FROM messages
         WHERE conversation_id = ? AND backend_turn_id = 'steward-report:turn-terminal-test'",
    )
    .bind(&steward_conversation_id)
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    let content: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(content["content"].as_str().unwrap().contains("已修改 7 处指代问题"));
    assert_eq!(content["_meta"]["steward_report"], true);
    assert_eq!(content["_meta"]["run_id"], "turn-terminal-test");
}

#[tokio::test]
async fn steward_bootstrap_injects_only_the_control_mcp_and_reuses_profile() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let assistant = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": "steward-test-assistant",
            "name": "Steward Test Assistant",
            "agent_id": "8e1acf31"
        }),
        &token,
        &csrf,
    );
    assert_eq!(
        app.clone().oneshot(assistant).await.unwrap().status(),
        StatusCode::CREATED
    );
    let replacement_assistant = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": "steward-replacement-assistant",
            "name": "Steward Replacement Assistant",
            "agent_id": "8e1acf31"
        }),
        &token,
        &csrf,
    );
    assert_eq!(
        app.clone().oneshot(replacement_assistant).await.unwrap().status(),
        StatusCode::CREATED
    );

    let bootstrap = json_with_token(
        "POST",
        "/api/steward/bootstrap",
        json!({"assistant_id": "steward-test-assistant"}),
        &token,
        &csrf,
    );
    let concurrent_bootstrap = json_with_token(
        "POST",
        "/api/steward/bootstrap",
        json!({"assistant_id": "steward-test-assistant"}),
        &token,
        &csrf,
    );
    let (first, second) = tokio::join!(
        app.clone().oneshot(bootstrap),
        app.clone().oneshot(concurrent_bootstrap)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(second.status(), StatusCode::CREATED);
    let first_body = body_json(first).await;
    let second_body = body_json(second).await;
    let conversation_id = first_body["data"]["conversation_id"].as_str().unwrap().to_owned();
    assert_eq!(second_body["data"]["conversation_id"], conversation_id);

    let conversation = get_with_token(&format!("/api/conversations/{conversation_id}"), &token);
    let response = app.clone().oneshot(conversation).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let extra = &body["data"]["extra"];
    assert_eq!(body["data"]["name"], "大管家");
    assert_eq!(body["data"]["name_source"], "user");
    assert_eq!(extra["steward"], true);
    assert_eq!(extra["session_mcp_servers"][0]["name"], "aionui-steward");
    assert_eq!(
        extra["session_mcp_servers"][0]["transport"]["args"],
        json!(["mcp-steward-stdio"])
    );

    let switch = json_with_token(
        "POST",
        "/api/steward/assistant",
        json!({"assistant_id": "steward-replacement-assistant"}),
        &token,
        &csrf,
    );
    let switched = body_json(app.clone().oneshot(switch).await.unwrap()).await;
    assert_eq!(switched["data"]["conversation_id"], conversation_id);
    assert_eq!(switched["data"]["assistant_id"], "steward-replacement-assistant");
    let persisted_assistant: String =
        sqlx::query_scalar("SELECT assistant_id FROM conversation_assistant_snapshots WHERE conversation_id = ?")
            .bind(&conversation_id)
            .fetch_one(services.database.pool())
            .await
            .unwrap();
    assert_eq!(persisted_assistant, "steward-replacement-assistant");
    let acp_session_id: Option<String> =
        sqlx::query_scalar("SELECT session_id FROM acp_session WHERE conversation_id = ?")
            .bind(&conversation_id)
            .fetch_one(services.database.pool())
            .await
            .unwrap();
    assert_eq!(acp_session_id, None);
    let embedded_leader_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM teams, json_each(teams.agents) \
         WHERE json_extract(value, '$.conversation_id') = ? AND json_extract(value, '$.role') = 'lead'",
    )
    .bind(&conversation_id)
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    assert_eq!(
        embedded_leader_count, 0,
        "the steward must not own a latent worker team"
    );

    let slash = get_with_token(&format!("/api/conversations/{conversation_id}/slash-commands"), &token);
    let slash_body = body_json(app.clone().oneshot(slash).await.unwrap()).await;
    assert!(
        slash_body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["command"] == "/tasks")
    );

    let direct_command = json_with_token(
        "POST",
        "/api/steward/commands",
        json!({"content": "/tasks"}),
        &token,
        &csrf,
    );
    let direct_body = body_json(app.clone().oneshot(direct_command).await.unwrap()).await;
    assert_eq!(direct_body["data"]["handled"], true);
    assert_eq!(direct_body["data"]["command"], "tasks");

    let conversation_command = json_with_token(
        "POST",
        &format!("/api/conversations/{conversation_id}/messages"),
        json!({"content": "有哪些任务"}),
        &token,
        &csrf,
    );
    let response = app.clone().oneshot(conversation_command).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let persisted_machine_messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
        .bind(&conversation_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(persisted_machine_messages, 4);

    // Embedded-team startup refreshes the assistant MCP binding. The control
    // MCP is conversation-owned and must survive that first real send path.
    let send = json_with_token(
        "POST",
        &format!("/api/conversations/{conversation_id}/messages"),
        json!({"content": "read-only steward smoke test"}),
        &token,
        &csrf,
    );
    assert_eq!(app.clone().oneshot(send).await.unwrap().status(), StatusCode::ACCEPTED);
    let conversation = get_with_token(&format!("/api/conversations/{conversation_id}"), &token);
    let body = body_json(app.clone().oneshot(conversation).await.unwrap()).await;
    assert_eq!(
        body["data"]["extra"]["session_mcp_servers"][0]["name"],
        "aionui-steward"
    );

    // Bootstrap is also the repair boundary for a profile created by an older
    // build that lost its control snapshot or accepted an agent-generated name.
    sqlx::query(
        "UPDATE conversations SET name = 'Drifted title', name_source = 'agent',
             extra = json_set(extra,
                 '$.preset_context', 'legacy steward prompt',
                 '$.session_mcp_servers', json('[]'),
                 '$.mcp_servers', json('[]'),
                 '$.mcp_statuses', json('[]'))
         WHERE id = ?",
    )
    .bind(&conversation_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    let repair = json_with_token(
        "POST",
        "/api/steward/bootstrap",
        json!({"assistant_id": "steward-replacement-assistant"}),
        &token,
        &csrf,
    );
    let repaired = body_json(app.clone().oneshot(repair).await.unwrap()).await;
    assert_eq!(repaired["data"]["conversation_name"], "大管家");
    let conversation = get_with_token(&format!("/api/conversations/{conversation_id}"), &token);
    let body = body_json(app.oneshot(conversation).await.unwrap()).await;
    assert_eq!(body["data"]["name_source"], "user");
    let stored_extra: String = sqlx::query_scalar("SELECT extra FROM conversations WHERE id = ?")
        .bind(&conversation_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let stored_extra: serde_json::Value = serde_json::from_str(&stored_extra).unwrap();
    assert!(
        stored_extra["preset_context"]
            .as_str()
            .unwrap()
            .contains("unregistered top-level conversations")
    );
    assert!(
        stored_extra["preset_context"]
            .as_str()
            .unwrap()
            .contains("Only archive or restore a conversation after the user explicitly requests that exact target")
    );
    assert!(
        stored_extra["preset_context"]
            .as_str()
            .unwrap()
            .contains("Never create or bind a replacement primary after a dispatch timeout")
    );
    assert!(
        stored_extra["preset_context"]
            .as_str()
            .unwrap()
            .contains("use steward_ask_task and include the returned leader reply")
    );
    assert_eq!(
        body["data"]["extra"]["session_mcp_servers"][0]["name"],
        "aionui-steward"
    );
}

#[tokio::test]
async fn steward_task_create_bind_resolve_and_overview_round_trip() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let conversation = json_with_token(
        "POST",
        "/api/conversations",
        json!({"type": "acp", "name": "登录稳定性修复", "extra": {}}),
        &token,
        &csrf,
    );
    let conversation_body = body_json(app.clone().oneshot(conversation).await.unwrap()).await;
    let conversation_id = conversation_body["data"]["id"].as_str().unwrap();

    let unregistered = json_with_token(
        "POST",
        "/api/conversations",
        json!({"type": "acp", "name": "尚未登记的顶层任务", "extra": {}}),
        &token,
        &csrf,
    );
    let unregistered_body = body_json(app.clone().oneshot(unregistered).await.unwrap()).await;
    let unregistered_id = unregistered_body["data"]["id"].as_str().unwrap().to_owned();

    let team_lead = json_with_token(
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "name": "旧版团队顶层 Leader",
            "extra": {"role": "lead", "teamId": "team-legacy"}
        }),
        &token,
        &csrf,
    );
    let team_lead_body = body_json(app.clone().oneshot(team_lead).await.unwrap()).await;
    let team_lead_id = team_lead_body["data"]["id"].as_str().unwrap().to_owned();

    for (name, extra) in [
        ("团队内部 Worker", json!({"role": "teammate", "teamId": "team-1"})),
        ("管家控制会话", json!({"steward": true})),
    ] {
        let hidden = json_with_token(
            "POST",
            "/api/conversations",
            json!({"type": "acp", "name": name, "extra": extra}),
            &token,
            &csrf,
        );
        assert_eq!(app.clone().oneshot(hidden).await.unwrap().status(), StatusCode::CREATED);
    }

    let create = json_with_token(
        "POST",
        "/api/steward/tasks",
        json!({
            "title": "登录稳定性修复",
            "objective": "修复登录中断并通过回归测试",
            "conversation_id": conversation_id
        }),
        &token,
        &csrf,
    );
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    let task_id = body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["data"]["execution_state"], "idle");
    assert_eq!(body["data"]["sessions"][0]["conversation_id"], conversation_id);

    let resolve = json_with_token(
        "POST",
        "/api/steward/tasks/resolve",
        json!({"objective": "登录稳定性修复"}),
        &token,
        &csrf,
    );
    let body = body_json(app.clone().oneshot(resolve).await.unwrap()).await;
    assert_eq!(body["data"][0]["task_id"], task_id);
    assert!(
        body["data"][0]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "exact_text")
    );

    let overview = get_with_token("/api/steward/overview", &token);
    let body = body_json(app.oneshot(overview).await.unwrap()).await;
    assert_eq!(body["data"]["open_tasks"], 1);
    assert_eq!(body["data"]["tasks"][0]["id"], task_id);
    let unregistered = body["data"]["unregistered_conversations"].as_array().unwrap();
    assert_eq!(unregistered.len(), 2);
    let ids = unregistered
        .iter()
        .filter_map(|conversation| conversation["conversation_id"].as_str())
        .collect::<Vec<_>>();
    assert!(ids.contains(&unregistered_id.as_str()));
    assert!(ids.contains(&team_lead_id.as_str()));
    assert!(
        unregistered
            .iter()
            .all(|conversation| conversation["conversation_name"] != "团队内部 Worker")
    );
}

#[tokio::test]
async fn steward_worker_binding_does_not_fake_a_primary_runtime() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let conversation = json_with_token(
        "POST",
        "/api/conversations",
        json!({"type": "acp", "name": "Worker only", "extra": {}}),
        &token,
        &csrf,
    );
    let conversation_body = body_json(app.clone().oneshot(conversation).await.unwrap()).await;
    let conversation_id = conversation_body["data"]["id"].as_str().unwrap();

    let create = json_with_token(
        "POST",
        "/api/steward/tasks",
        json!({"title": "Unassigned task", "objective": "Needs a primary session"}),
        &token,
        &csrf,
    );
    let task_body = body_json(app.clone().oneshot(create).await.unwrap()).await;
    let task_id = task_body["data"]["id"].as_str().unwrap();

    let bind_worker = json_with_token(
        "POST",
        &format!("/api/steward/tasks/{task_id}/sessions"),
        json!({"conversation_id": conversation_id, "role": "worker"}),
        &token,
        &csrf,
    );
    let response = app.oneshot(bind_worker).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["execution_state"], "unassigned");
    assert_eq!(body["data"]["sessions"][0]["role"], "worker");
}

#[tokio::test]
async fn steward_primary_replacement_requires_explicit_override() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let first = json_with_token(
        "POST",
        "/api/conversations",
        json!({"type": "acp", "name": "Original primary", "extra": {}}),
        &token,
        &csrf,
    );
    let first_body = body_json(app.clone().oneshot(first).await.unwrap()).await;
    let first_id = first_body["data"]["id"].as_str().unwrap();
    let second = json_with_token(
        "POST",
        "/api/conversations",
        json!({"type": "acp", "name": "Replacement candidate", "extra": {}}),
        &token,
        &csrf,
    );
    let second_body = body_json(app.clone().oneshot(second).await.unwrap()).await;
    let second_id = second_body["data"]["id"].as_str().unwrap();

    let create = json_with_token(
        "POST",
        "/api/steward/tasks",
        json!({
            "title": "Protected primary task",
            "objective": "Keep using the original conversation",
            "conversation_id": first_id
        }),
        &token,
        &csrf,
    );
    let task_body = body_json(app.clone().oneshot(create).await.unwrap()).await;
    let task_id = task_body["data"]["id"].as_str().unwrap();

    let implicit_replace = json_with_token(
        "POST",
        &format!("/api/steward/tasks/{task_id}/sessions"),
        json!({"conversation_id": second_id, "role": "primary"}),
        &token,
        &csrf,
    );
    let response = app.clone().oneshot(implicit_replace).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["code"], "CONFLICT");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("explicit replace_primary=true is required")
    );

    let explicit_replace = json_with_token(
        "POST",
        &format!("/api/steward/tasks/{task_id}/sessions"),
        json!({
            "conversation_id": second_id,
            "role": "primary",
            "replace_primary": true
        }),
        &token,
        &csrf,
    );
    let response = app.oneshot(explicit_replace).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["sessions"][0]["conversation_id"], second_id);
}
