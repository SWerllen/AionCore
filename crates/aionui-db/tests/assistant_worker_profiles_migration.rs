use aionui_db::init_database_memory;

#[tokio::test]
async fn worker_profiles_are_user_scoped_and_reference_assistant_definitions() {
    let database = init_database_memory().await.expect("init database");
    let pool = database.pool();

    sqlx::query(
        r#"
        INSERT INTO assistant_definitions (
            id, assistant_id, source, owner_type, source_ref,
            name, name_i18n, description_i18n, avatar_type, agent_id,
            rule_resource_type, recommended_prompts, recommended_prompts_i18n,
            default_model_mode, default_permission_mode,
            default_thought_level_mode, default_skills_mode, default_skill_ids,
            custom_skill_names, default_disabled_builtin_skill_ids,
            default_mcps_mode, default_mcp_ids, created_at, updated_at
        ) VALUES (
            'definition-codex', 'codex-test', 'builtin', 'system', 'codex-test',
            'Codex Test', '{}', '{}', 'none', 'codex',
            'none', '[]', '{}',
            'auto', 'auto', 'auto', 'auto', '[]',
            '[]', '[]', 'auto', '[]', 1, 1
        )
        "#,
    )
    .execute(pool)
    .await
    .expect("seed assistant definition");
    let definition_id = "definition-codex".to_string();

    for (id, user_id, name) in [
        ("profile-user-a", "system_default_user", "Balanced"),
        ("profile-user-b", "system_default_user", "Economy"),
    ] {
        sqlx::query(
            "INSERT INTO assistant_worker_profiles (
                id, user_id, assistant_definition_id, name, model_id, reasoning_effort, context_window,
                difficulty_ceiling, estimated_cost_micros, currency, enabled, sort_order,
                created_at, updated_at
             ) VALUES (?, ?, ?, ?, 'gpt-test', 'high', 128000, 4, 12500000, 'CNY', 1, 0, 1, 1)",
        )
        .bind(id)
        .bind(user_id)
        .bind(&definition_id)
        .bind(name)
        .execute(pool)
        .await
        .expect("insert worker profile");
    }

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM assistant_worker_profiles
         WHERE user_id = 'system_default_user' AND assistant_definition_id = ?",
    )
    .bind(&definition_id)
    .fetch_one(pool)
    .await
    .expect("count profiles");
    assert_eq!(count, 2);

    let context_window: Option<i64> =
        sqlx::query_scalar("SELECT context_window FROM assistant_worker_profiles WHERE id = 'profile-user-a'")
            .fetch_one(pool)
            .await
            .expect("read worker context window");
    assert_eq!(context_window, Some(128_000));

    let invalid_difficulty = sqlx::query(
        "INSERT INTO assistant_worker_profiles (
            id, user_id, assistant_definition_id, name, model_id, difficulty_ceiling,
            estimated_cost_micros, currency, enabled, sort_order, created_at, updated_at
         ) VALUES ('invalid-difficulty', 'system_default_user', ?, 'Invalid', 'gpt-test', 6, 0, 'CNY', 1, 0, 1, 1)",
    )
    .bind(&definition_id)
    .execute(pool)
    .await;
    assert!(invalid_difficulty.is_err());
}
