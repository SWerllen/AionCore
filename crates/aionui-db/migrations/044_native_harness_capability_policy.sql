-- Codex and Qoder keep their installed harness as the source of ordinary
-- Skills and MCP servers. AionUi may still attach its ephemeral Team MCP so
-- team mailbox/task communication remains available.
--
-- The policy lives in behavior_policy to stay row-scoped and forward-compatible
-- with custom/extension agents. json_set preserves existing behavior flags.
UPDATE agent_metadata
SET behavior_policy = json_set(
        COALESCE(behavior_policy, '{}'),
        '$.skill_policy', 'native_only',
        '$.mcp_policy', 'native_plus_team',
        '$.harness_policy', 'preserve'
    ),
    updated_at = unixepoch('now','subsec')*1000
WHERE backend IN ('codex', 'qoder');
