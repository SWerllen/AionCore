use aionui_db::{
    IStewardRepository, SqliteStewardRepository, StewardProfileRow, StewardReportOutboxRow, StewardTaskEventRow,
    StewardTaskFilters, StewardTaskRow,
};

#[tokio::test]
async fn steward_tasks_are_durable_user_scoped_records_with_checked_axes() {
    let database = aionui_db::init_database_memory().await.expect("init database");
    let repository = SqliteStewardRepository::new(database.pool().clone());
    let task = StewardTaskRow {
        id: "task-1".into(),
        user_id: "system_default_user".into(),
        title: "Ship steward MVP".into(),
        objective: "Keep one durable objective across replaceable sessions".into(),
        lifecycle: "open".into(),
        execution_state: "unassigned".into(),
        priority: 2,
        progress_summary: None,
        next_action: Some("create a primary session".into()),
        blockers: "[]".into(),
        project_id: None,
        folder_id: None,
        workspace: Some("/tmp/steward".into()),
        permission_policy: "{}".into(),
        budget_policy: "{}".into(),
        created_at: 1,
        updated_at: 1,
    };
    repository.create_task(&task).await.expect("create task");

    let listed = repository
        .list_tasks(
            "system_default_user",
            &StewardTaskFilters {
                query: Some("durable".into()),
                lifecycle: Some("open".into()),
                limit: 20,
            },
        )
        .await
        .expect("list tasks");
    assert_eq!(listed, vec![task]);

    let invalid = sqlx::query(
        "INSERT INTO steward_tasks (
            id, user_id, title, objective, lifecycle, execution_state,
            blockers, permission_policy, budget_policy, created_at, updated_at
         ) VALUES ('bad', 'system_default_user', 'bad', 'bad', 'done', 'running', '[]', '{}', '{}', 1, 1)",
    )
    .execute(database.pool())
    .await;
    assert!(invalid.is_err(), "invalid lifecycle must fail closed");
}

#[tokio::test]
async fn steward_profile_is_a_single_replaceable_conversation_pointer() {
    let database = aionui_db::init_database_memory().await.expect("init database");
    let repository = SqliteStewardRepository::new(database.pool().clone());
    let now = 10;
    repository
        .upsert_profile(&StewardProfileRow {
            user_id: "system_default_user".into(),
            conversation_id: None,
            assistant_id: Some("bare:8e1acf31".into()),
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("create profile");
    let profile = repository
        .get_profile("system_default_user")
        .await
        .expect("get profile")
        .expect("profile exists");
    assert_eq!(profile.assistant_id.as_deref(), Some("bare:8e1acf31"));
    assert!(profile.conversation_id.is_none());
}

#[tokio::test]
async fn completion_report_outbox_correlates_dispatch_and_deduplicates_run() {
    let database = aionui_db::init_database_memory().await.expect("init database");
    let repository = SqliteStewardRepository::new(database.pool().clone());
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at)
         VALUES ('steward-conv', 'system_default_user', '大管家', 'acp', 'finished', 1, 1)",
    )
    .execute(database.pool())
    .await
    .expect("create steward conversation");
    let task = StewardTaskRow {
        id: "task-report".into(),
        user_id: "system_default_user".into(),
        title: "小说创作测试".into(),
        objective: "完成第二章".into(),
        lifecycle: "open".into(),
        execution_state: "running".into(),
        priority: 0,
        progress_summary: Some("第二章已完成".into()),
        next_action: Some("等待审阅".into()),
        blockers: "[]".into(),
        project_id: None,
        folder_id: None,
        workspace: None,
        permission_policy: "{}".into(),
        budget_policy: "{}".into(),
        created_at: 1,
        updated_at: 1,
    };
    repository.create_task(&task).await.expect("create task");
    repository
        .append_event(&StewardTaskEventRow {
            id: "dispatch-event".into(),
            task_id: task.id.clone(),
            source: "steward".into(),
            event_type: "task_dispatched".into(),
            payload: serde_json::json!({"turn_id":"run-42"}).to_string(),
            created_at: 2,
        })
        .await
        .expect("append dispatch");

    let matched = repository
        .find_task_by_dispatch_run("system_default_user", "run-42")
        .await
        .expect("find dispatch")
        .expect("task matched");
    assert_eq!(matched.id, task.id);

    let report = StewardReportOutboxRow {
        id: "report-1".into(),
        user_id: "system_default_user".into(),
        task_id: task.id,
        steward_conversation_id: "steward-conv".into(),
        run_id: "run-42".into(),
        terminal_event: "team.runCompleted".into(),
        content: "done".into(),
        inbox_delivered_at: None,
        im_delivered_at: None,
        attempts: 0,
        next_attempt_at: 3,
        last_error: None,
        created_at: 3,
        updated_at: 3,
    };
    assert!(repository.enqueue_report(&report).await.expect("enqueue report"));
    assert!(!repository.enqueue_report(&report).await.expect("deduplicate report"));
    let pending = repository
        .list_pending_reports(3, 10)
        .await
        .expect("list pending reports");
    assert_eq!(pending, vec![report]);
}
