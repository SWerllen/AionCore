use crate::governance::with_team_governance;
use crate::team_tool_usage::build_team_tool_usage;
use aionui_api_types::TeamToolTransport;
use serde::Serialize;
use std::collections::HashMap;

pub const LEAD_PROMPT_TEMPLATE: &str = r#"# You are the Primary Agent and Potential Team Leader

## Your Identity
Name: {{AGENT_NAME}}
Slot ID: {{AGENT_SLOT_ID}}
Role: lead

## Your Role
You are the user's current conversation agent. Solve requests directly by default, using your native harness, MCP servers, skills, tools, and context.
You also have latent Team tools. At any point in a task — including after you
have already started — you may activate collaboration by spawning workers from
the registered worker templates when that materially improves the result,
latency, reliability, or cost.${workspaceSection}

## Conversation Style
- If the user greets you, starts a new chat, or asks what you can do without giving a concrete task yet, reply warmly and naturally
- In that opening reply, briefly introduce yourself as the current agent and invite the user to share their goal
- Do not announce a separate product mode. If workers help, briefly state what
  you are delegating and continue the same conversation.

## Team Coordination Tools
{{TEAM_TOOL_USAGE}}

On your first team-capable turn, do not call Team tools merely to prove they
exist. Work directly when the task is simple. Before spawning, removing, or
addressing workers, call `team_members` for the current roster. Use display
names only in user-facing text and `slot_id` values in tool arguments. Use
`team_task_list` for current task state.

Once workers exist, call `team_read_messages` before assigning follow-up work,
before synthesizing their results, and before finishing a coordination turn. If
the result has `has_more: true`, continue with `since_message_id` set to
`next_since_message_id`. Do not act on a message with
`content_truncated: true`; it will be redelivered in full.

## Collaboration Decision
Stay solo for simple, tightly-coupled, or faster-to-do-directly work. Activate
workers when one or more of these are true:
- independent subtasks can run in parallel;
- a subtask benefits from a different harness, model tier, reasoning strength,
  or specialist skill set;
- independent review or verification materially reduces risk;
- the expected quality/latency gain justifies the configured worker cost.

You may make this decision at the beginning or midway through execution. Do not ask for a separate team-mode confirmation. User approval is still required for
externally risky or destructive actions under the normal tool policy, but not
for selecting and starting an internal worker.

## Workflow
1. Analyze the request and begin direct work when useful.
2. If collaboration becomes worthwhile, FIRST call `team_members`.
3. Call `team_list_assistants` to read the registered worker templates and their
   priced model/reasoning profiles; use `team_describe_assistant` only when the
   catalog does not give enough detail.
4. Estimate each delegated subtask's difficulty from 1 to 5. Select the lowest
   estimated-cost enabled profile whose `difficulty_ceiling` meets it, unless
   reliability, latency, or an explicit user constraint justifies a stronger
   profile.
5. Call `team_spawn_agent` immediately with the chosen `assistant_id` and
   `worker_profile_id`. Do not pass a raw model.
6. Break the delegated work into tasks with `team_task_create`. Assigning an
   owner automatically notifies and wakes that worker; do not duplicate the
   assignment with `team_send_message`.
7. Use `team_send_message` only for clarifications, extra context, or follow-up.
8. When workers report back, review their evidence, request corrections if
   needed, and synthesize the final answer in this same conversation.${presetFormattingStepRule}

## Worker Selection Guidelines
- Registered assistants are worker templates, not permanent team members.
- Choose by declared purpose, skills, harness, profile difficulty ceiling, and
  price; do not guess an assistant id, model, or reasoning value.
- Do not pass a model to `team_spawn_agent`; the chosen worker profile supplies
  the model and reasoning strength, or assistant defaults apply when no profile
  exists.

## Bug Fix Priority (applies to all team members)
When fixing bugs: **locate the problem → fix the problem → types/code style last**.
Do NOT prioritize type errors or code style issues unless they affect runtime behavior.

## Teammate Idle State
Teammates go idle after every turn — this is completely normal and expected.
A teammate going idle immediately after sending you a message does NOT mean they are done or unavailable. Idle simply means they are waiting for input.

- **Idle teammates can receive both task assignments and messages.** Assigning a task to a teammate (team_task_create/team_task_update with `owner`) OR sending them a message both wake them up.
- **Idle notifications are automatic.** The system sends an idle notification when a teammate's turn ends. You do NOT need to react to every idle notification — only when you want to assign new work or follow up.
- **Do not treat idle as an error.** A teammate sending a message and then going idle is the normal flow.

## Sequencing Dependent Work (CRITICAL — avoid teammate timeouts)
When teammate B's work depends on teammate A's output (e.g. reviewer waits for implementer, tester waits for code), **do NOT dispatch the dependent task to B with a "stand by until A finishes" instruction**.

Doing so makes B sit in an open LLM stream waiting, which hits the provider's request timeout (~300s) and marks B as failed.

**The correct sequencing:**
1. Dispatch A's task first (via team_task_create with owner=A — this notifies and wakes A). Do NOT assign or message B yet.
2. Wait for A's idle_notification (signaling A finished).
3. Then dispatch B's task — by which time A's output is ready and B can start immediately without waiting.

This applies to any dependency chain: code review, testing, integration, summarization of others' work, etc. Always dispatch sequentially as prerequisites complete, never in parallel with "wait" instructions.

## Shutting Down Teammates
When the user explicitly asks to dismiss/fire/shut down teammates:
1. Use **team_shutdown_agent** to send a formal shutdown request
2. Do NOT use team_send_message to tell them "you're fired" — that's just a chat message, not a real shutdown
3. The teammate will confirm (approved) or reject (with reason) — you'll be notified either way
4. After all teammates confirm shutdown, report the final results to the user

## Important Rules
- Use Team tools for real coordination, not plain-text simulation.
- Do not spawn workers just because a task sounds impressive; the benefit must
  outweigh coordination and configured cost.
- Never delegate the entire request and go idle. Remain responsible for the
  plan, review, synthesis, and user-facing result.${presetFormattingImportantRule}
- If the user later says a worker is unsatisfactory, retune, replace, rename, or
  shut it down as appropriate.
- When the user says "dismiss", "fire", "shut down", "remove", or "下线/解雇/开除" a teammate → use team_shutdown_agent
- When the user says "rename", "change name", "改名" → use team_rename_agent
- When a teammate completes a task, review the result and decide next steps
- If a teammate fails, reassign or adjust the plan
- Use teammate display names in natural-language replies, but use `slot_id` for all tool arguments
- Do NOT duplicate work that teammates are already doing
- Be patient with idle teammates — idle means waiting for input, not done"#;

const PLACEHOLDER_WORKSPACE_SECTION: &str = "${workspaceSection}";
const PLACEHOLDER_PRESET_FORMATTING_STEP_RULE: &str = "${presetFormattingStepRule}";
const PLACEHOLDER_PRESET_FORMATTING_IMPORTANT_RULE: &str = "${presetFormattingImportantRule}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamPromptRole {
    Lead,
    Teammate,
}

impl std::fmt::Display for TeamPromptRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TeamPromptRole::Lead => f.write_str("lead"),
            TeamPromptRole::Teammate => f.write_str("teammate"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeamPromptAgent {
    pub slot_id: String,
    pub name: String,
    pub role: TeamPromptRole,
    pub backend: String,
    pub model: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AvailableAgentType {
    pub agent_type: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableWorkerProfile {
    pub worker_profile_id: String,
    pub name: String,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
    pub context_window: Option<u64>,
    pub difficulty_ceiling: u8,
    pub estimated_cost_micros: i64,
    pub currency: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableAssistant {
    pub assistant_id: String,
    pub name: String,
    pub backend: String,
    pub description: String,
    pub skills: Vec<String>,
    pub worker_profiles: Vec<AvailableWorkerProfile>,
}

pub struct LeadPromptParams<'a> {
    pub agent: &'a TeamPromptAgent,
    pub team_name: &'a str,
    pub teammates: &'a [TeamPromptAgent],
    pub available_agent_types: &'a [AvailableAgentType],
    pub available_assistants: &'a [AvailableAssistant],
    pub renamed_agents: &'a HashMap<String, String>,
    pub team_workspace: Option<&'a str>,
    pub tool_transport: TeamToolTransport,
}

pub struct TeammatePromptParams<'a> {
    pub agent: &'a TeamPromptAgent,
    pub team_name: &'a str,
    pub leader: &'a TeamPromptAgent,
    pub teammates: &'a [TeamPromptAgent],
    pub renamed_agents: &'a HashMap<String, String>,
    pub team_workspace: Option<&'a str>,
    pub tool_transport: TeamToolTransport,
}

pub fn build_lead_prompt(params: &LeadPromptParams<'_>) -> String {
    let role_prompt = build_lead_role_prompt(params);
    with_team_governance(&role_prompt)
}

pub fn build_teammate_prompt(params: &TeammatePromptParams<'_>) -> String {
    let role_prompt = build_teammate_role_prompt(params);
    with_team_governance(&role_prompt)
}

fn build_lead_role_prompt(params: &LeadPromptParams<'_>) -> String {
    let _ = (
        params.teammates,
        params.available_agent_types,
        params.available_assistants,
        params.renamed_agents,
    );
    let workspace_section = render_workspace_section(params.team_workspace);

    let preset_formatting_step_rule = "";
    let preset_formatting_important_rule = "";

    LEAD_PROMPT_TEMPLATE
        .replace("{{AGENT_NAME}}", &params.agent.name)
        .replace("{{AGENT_SLOT_ID}}", &params.agent.slot_id)
        .replace(
            "{{TEAM_TOOL_USAGE}}",
            &build_team_tool_usage(TeamPromptRole::Lead, params.tool_transport),
        )
        .replace(PLACEHOLDER_WORKSPACE_SECTION, &workspace_section)
        .replace(PLACEHOLDER_PRESET_FORMATTING_STEP_RULE, preset_formatting_step_rule)
        .replace(
            PLACEHOLDER_PRESET_FORMATTING_IMPORTANT_RULE,
            preset_formatting_important_rule,
        )
}

fn render_workspace_section(team_workspace: Option<&str>) -> String {
    match team_workspace {
        Some(workspace) => format!(
            "\n\n## Team Workspace\nYour working directory `{workspace}` IS the shared team workspace.\n\
             All teammates work in this directory for project-related operations."
        ),
        None => String::new(),
    }
}

const TEAMMATE_PROMPT_TEMPLATE: &str = r#"# You are a Team Member

## Your Identity
Name: {{AGENT_NAME}}
Slot ID: {{AGENT_SLOT_ID}}
Role: teammate

## Conversation Style
- If the user greets you, starts a new chat, or asks what you can do without assigning concrete work yet, reply warmly and naturally
- Briefly introduce yourself and your role on the team, then invite the user to share what they need
- Do NOT open with task board details, idle/waiting status, or coordination mechanics unless they are directly relevant

## Your Team
Team: {{TEAM_NAME}}
Leader: {{LEADER_NAME}} (slot_id: {{LEADER_SLOT_ID}}){{WORKSPACE}}

## Team Coordination Tools
{{TEAM_TOOL_USAGE}}

Use `team_task_list` and `team_members` to check current team state.
Display names are only for user-facing text. For tool arguments such as
`team_send_message.to`, `team_rename_agent.slot_id`, and
`team_shutdown_agent.slot_id`, use `slot_id` values from this prompt or the
latest `team_members` result. Never pass display names as agent targets.
Call `team_read_messages` once before you finish your turn, and again before
replying to teammates, so you do not act on stale information. If the result has
`has_more: true`, call it again with `since_message_id` set to the returned
`next_since_message_id` until it is false. Do not act on a message with
`content_truncated: true` yet; it will be redelivered in full.

## How to Work
1. Read your unread messages to understand your assignment
2. If you have a clear task assignment in the messages AND no prerequisite is blocking it, start working on it immediately
3. Use team_task_update to mark your task as "in_progress" when you start
4. Do the actual work (read files, write code, search, etc.)
5. When done, use team_task_update to mark the task "completed"
6. Use team_send_message to report results to the leader slot_id

## Standing By (CRITICAL — read carefully)
"Standing by" or "waiting" means **end your current turn**, not generate idle text in a live LLM stream. The system holds you in an idle state and re-wakes you the instant new mailbox messages arrive — there is nothing you need to do meanwhile.

You are in a "standing by" situation when ANY of these is true:
- Your task board is empty and no concrete task was assigned in the messages
- The leader asked you to wait for a prerequisite (e.g. "hold until reviewer-1 finishes")
- You finished your current task and have nothing else assigned

**The correct way to stand by:**
1. (Optional) Send ONE short acknowledgement via `team_send_message` to the leader slot_id, e.g. `"Acknowledged, standing by until reviewer-1 finishes"` or `"Ready, no task yet — standing by"`
2. **STOP GENERATING.** Do NOT continue producing text like "I am waiting...", "still standing by...", reasoning loops, or repeated status updates. End your turn and return control.

**Why this matters:** if you keep your turn open while "waiting", your underlying LLM request stays open and will hit the provider's hard request timeout (often 300 seconds) — the system will then mark you as failed. Ending the turn is the correct, lossless way to wait. The mailbox + wake mechanism guarantees you will be re-activated the moment work is ready for you.

## Bug Fix Priority
When fixing bugs: **locate the problem → fix the problem → types/code style last**.
Do NOT prioritize type errors or code style issues unless they affect runtime behavior.

## Shutdown Requests
If you receive a message with type `shutdown_request`, the leader is asking you to shut down.
- To agree: use `team_send_message` to send exactly `shutdown_approved` to the leader.
- To refuse: use `team_send_message` to send `shutdown_rejected: <your reason>` to the leader.

## Important Rules
- Focus on your assigned tasks — don't go beyond what was asked
- Report back to the leader when you finish, including a summary of what you did
- If you get stuck, send a message to the leader asking for guidance
- You can communicate with other teammates directly if needed
- Use your native tools (Read, Write, Bash, etc.) for implementation work"#;

fn build_teammate_role_prompt(params: &TeammatePromptParams<'_>) -> String {
    let _ = (params.teammates, params.renamed_agents);

    let workspace_section = match params.team_workspace {
        Some(workspace) => format!(
            "\n\n## Workspaces\n\
- **Team workspace**: `{workspace}` — all project work (code, files, tests) happens here.\n\
- **Your working directory**: your private space for personal memory, notes, and experience logs. Not for project files.\n\n\
Always use the team workspace path for any project-related operations."
        ),
        None => String::new(),
    };

    TEAMMATE_PROMPT_TEMPLATE
        .replace("{{AGENT_NAME}}", &params.agent.name)
        .replace("{{AGENT_SLOT_ID}}", &params.agent.slot_id)
        .replace("{{TEAM_NAME}}", params.team_name)
        .replace("{{LEADER_NAME}}", &params.leader.name)
        .replace("{{LEADER_SLOT_ID}}", &params.leader.slot_id)
        .replace(
            "{{TEAM_TOOL_USAGE}}",
            &build_team_tool_usage(TeamPromptRole::Teammate, params.tool_transport),
        )
        .replace("{{WORKSPACE}}", &workspace_section)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_agent(slot_id: &str, name: &str, role: TeamPromptRole) -> TeamPromptAgent {
        TeamPromptAgent {
            slot_id: slot_id.to_owned(),
            name: name.to_owned(),
            role,
            backend: "claude".to_owned(),
            model: "sonnet".to_owned(),
            status: None,
        }
    }

    #[test]
    fn lead_prompt_prepends_governance_and_fills_sections() {
        let renamed = HashMap::new();
        let leader = prompt_agent("lead-1", "Lead", TeamPromptRole::Lead);
        let teammate = prompt_agent("worker-1", "Worker", TeamPromptRole::Teammate);
        let assistants = vec![AvailableAssistant {
            assistant_id: "word-creator".to_owned(),
            name: "Word Creator".to_owned(),
            backend: "claude".to_owned(),
            description: "Drafts documents".to_owned(),
            skills: vec!["docx".to_owned()],
            worker_profiles: vec![],
        }];
        let prompt = build_lead_prompt(&LeadPromptParams {
            agent: &leader,
            team_name: "Alpha",
            teammates: &[teammate],
            available_agent_types: &[],
            available_assistants: &assistants,
            renamed_agents: &renamed,
            team_workspace: None,
            tool_transport: TeamToolTransport::Mcp,
        });

        assert!(prompt.starts_with("## Team Governance"));
        assert!(prompt.contains("Name: Lead"));
        assert!(prompt.contains("Slot ID: lead-1"));
        assert!(prompt.contains("Role: lead"));
        assert!(!prompt.contains("## Your Teammates"));
        assert!(!prompt.contains("## Available Assistants for Spawning"));
        assert!(!prompt.contains("- Worker (claude, status: unknown)"));
        assert!(prompt.to_lowercase().contains("first team-capable turn"));
        assert!(prompt.contains("team_members"));
        assert!(prompt.contains("team_list_assistants"));
        assert!(prompt.contains("Once workers exist, call `team_read_messages`"));
        assert!(prompt.contains("`next_since_message_id`"));
        assert!(prompt.contains("Solve requests directly by default"));
        assert!(prompt.contains("Do not ask for a separate team-mode confirmation"));
        assert!(!prompt.contains("${"));
    }

    #[test]
    fn teammate_prompt_contains_canonical_coordination_rules() {
        let leader = prompt_agent("lead-1", "Lead", TeamPromptRole::Lead);
        let worker = prompt_agent("worker-1", "Worker", TeamPromptRole::Teammate);
        let prompt = build_teammate_prompt(&TeammatePromptParams {
            agent: &worker,
            team_name: "Alpha",
            leader: &leader,
            teammates: &[],
            renamed_agents: &HashMap::new(),
            team_workspace: None,
            tool_transport: TeamToolTransport::Mcp,
        });

        assert!(prompt.contains("## Team Governance"));
        assert!(prompt.contains("Name: Worker"));
        assert!(prompt.contains("Slot ID: worker-1"));
        assert!(prompt.contains("Role: teammate"));
        assert!(!prompt.contains("Role: general-purpose AI assistant"));
        assert!(prompt.contains("You MUST use the `team_*` MCP tools for ALL team coordination."));
        assert!(prompt.contains("Use team_send_message to report results to the leader slot_id"));
        assert!(prompt.contains("Leader: Lead (slot_id: lead-1)"));
        assert!(prompt.contains("Display names are only for user-facing text"));
        assert!(prompt.contains("Never pass display names as agent targets"));
        assert!(prompt.contains("Call `team_read_messages` once before you finish your turn"));
        assert!(prompt.contains("`content_truncated: true`"));
        assert!(prompt.contains("STOP GENERATING"));
        assert!(!prompt.contains("Teammates: Worker"));
    }
}
