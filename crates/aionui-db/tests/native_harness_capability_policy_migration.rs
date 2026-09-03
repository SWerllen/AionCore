use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn codex_and_qoder_preserve_native_harness_capabilities() {
    let pool = init_database_memory().await.expect("init database");
    let repo = SqliteAgentMetadataRepository::new(pool.pool().clone());

    for backend in ["codex", "qoder"] {
        let row = repo
            .find_builtin_by_backend(backend)
            .await
            .expect("query builtin agent")
            .expect("builtin agent exists");
        let policy: serde_json::Value = serde_json::from_str(
            row.behavior_policy
                .as_deref()
                .expect("native harness agent has behavior policy"),
        )
        .expect("behavior policy is valid JSON");

        assert_eq!(policy["skill_policy"], "native_only", "{backend} skill policy");
        assert_eq!(policy["mcp_policy"], "native_plus_team", "{backend} MCP policy");
        assert_eq!(policy["harness_policy"], "preserve", "{backend} harness policy");
    }
}

#[tokio::test]
async fn unrelated_agents_keep_managed_capability_defaults() {
    let pool = init_database_memory().await.expect("init database");
    let repo = SqliteAgentMetadataRepository::new(pool.pool().clone());
    let claude = repo
        .find_builtin_by_backend("claude")
        .await
        .expect("query claude")
        .expect("claude exists");
    let policy: serde_json::Value =
        serde_json::from_str(claude.behavior_policy.as_deref().unwrap_or("{}")).expect("behavior policy is valid JSON");

    assert!(policy.get("skill_policy").is_none());
    assert!(policy.get("mcp_policy").is_none());
    assert!(policy.get("harness_policy").is_none());
}
