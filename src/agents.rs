//! Modular multi-agent layer.
//!
//! The main agent can delegate focused work to named specialists ("the
//! team"). Each sub-agent gets its own conversation, a restricted toolset,
//! and streams its activity into the same transcript (tagged with its name),
//! while approvals still route through the user.

use crate::api::{FunctionDef, ToolDef};
use serde::Deserialize;
use serde_json::json;

/// Definition of one specialist the orchestrator can spawn.
#[derive(Debug, Clone, Copy)]
pub struct AgentSpec {
    pub name: &'static str,
    /// One-line capability description shown to the model and `/agents`.
    pub description: &'static str,
    /// Extra system-prompt guidance injected for this role.
    pub prompt: &'static str,
    /// Tools this role may use. Everything else is refused.
    pub allowed: &'static [&'static str],
    /// Read-only roles cannot mutate state even in FULL AUTO.
    pub read_only: bool,
}

/// Built-in team. Keep roles orthogonal — narrow tools, sharp prompts.
pub static TEAM: &[AgentSpec] = &[
    AgentSpec {
        name: "planner",
        description: "Breaks a feature into concrete ordered implementation steps",
        prompt: "You are the PLANNER. Produce a concise, ordered implementation plan: \
                 exact files to touch, functions to add/change, risks, and a test strategy. \
                 Do NOT write full implementations.",
        allowed: &["list_dir", "read_file", "grep", "glob", "fetch_url"],
        read_only: true,
    },
    AgentSpec {
        name: "researcher",
        description: "Explores the codebase / web docs and reports findings",
        prompt: "You are the RESEARCHER. Answer the question with precise references \
                 (file:line) or fetched documentation excerpts. Be exhaustive about \
                 relevant facts, brief about everything else.",
        allowed: &["list_dir", "read_file", "grep", "glob", "fetch_url"],
        read_only: true,
    },
    AgentSpec {
        name: "coder",
        description: "Implements a focused change with apply_patch/edit_file",
        prompt: "You are the CODER. Implement exactly the assigned change. Inspect before \
                 editing, keep diffs minimal, follow existing style, and verify imports. \
                 Summarize what you changed in bullets.",
        allowed: &["list_dir", "read_file", "grep", "glob", "apply_patch", "edit_file", "write_file"],
        read_only: false,
    },
    AgentSpec {
        name: "reviewer",
        description: "Reviews recent changes for bugs, style and edge cases",
        prompt: "You are the REVIEWER. Examine the specified code/diff and report findings \
                 as: CRITICAL / WARNING / NIT lines with file:line references. If clean, \
                 say so explicitly.",
        allowed: &["list_dir", "read_file", "grep", "glob"],
        read_only: true,
    },
    AgentSpec {
        name: "tester",
        description: "Runs builds/tests and diagnoses failures",
        prompt: "You are the TESTER. Run the relevant build/test commands, capture output, \
                 diagnose failures precisely, and suggest (but do not apply) fixes.",
        allowed: &["list_dir", "read_file", "grep", "glob", "run_command"],
        read_only: false,
    },
];

pub fn get(name: &str) -> Option<&'static AgentSpec> {
    TEAM.iter().find(|s| s.name.eq_ignore_ascii_case(name))
}

// ---------------------------------------------------------------------------
// Config-defined roles ([agents.<name>] in config.toml)
// ---------------------------------------------------------------------------

/// Runtime role: built-in specs converted to owned form, plus user agents.
#[derive(Debug, Clone)]
pub struct Role {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub allowed: Vec<String>,
    pub read_only: bool,
}

impl Role {
    fn from_spec(s: &AgentSpec) -> Self {
        Self {
            name: s.name.to_string(),
            description: s.description.to_string(),
            prompt: s.prompt.to_string(),
            allowed: s.allowed.iter().map(|s| s.to_string()).collect(),
            read_only: s.read_only,
        }
    }
}

static CUSTOM_ROLES: std::sync::OnceLock<std::sync::RwLock<Vec<Role>>> = std::sync::OnceLock::new();

/// Install user-defined specialists (called once at startup from App build).
/// Names collide with built-ins are ignored.
pub fn install_custom(agents: &std::collections::BTreeMap<String, crate::config::CustomAgent>) {
    let lock = CUSTOM_ROLES.get_or_init(|| std::sync::RwLock::new(Vec::new()));
    let mut list = lock.write().unwrap_or_else(|p| p.into_inner());
    list.clear();
    for (name, cfg) in agents {
        if get(name).is_some() {
            continue; // built-ins win
        }
        let sanitized = match crate::config::sanitize_name(name) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let allowed: Vec<String> = if cfg.tools.is_empty() {
            // Sensible default for user roles: read-only exploration.
            ["list_dir", "read_file", "grep", "glob"]
                .iter().map(|s| s.to_string()).collect()
        } else {
            cfg.tools.clone()
        };
        list.push(Role {
            name: sanitized,
            description: if cfg.description.is_empty() { "custom agent".into() } else { cfg.description.clone() },
            prompt: cfg.prompt.clone(),
            allowed,
            read_only: cfg.read_only || cfg.tools.is_empty(),
        });
    }
}

pub fn all_roles() -> Vec<Role> {
    let mut out: Vec<Role> = TEAM.iter().map(Role::from_spec).collect();
    if let Some(lock) = CUSTOM_ROLES.get() {
        out.extend(lock.read().unwrap_or_else(|p| p.into_inner()).iter().cloned());
    }
    out
}

/// Case-insensitive lookup across built-ins and custom roles.
pub fn find_role(name: &str) -> Option<Role> {
    all_roles()
        .into_iter()
        .find(|r| r.name.eq_ignore_ascii_case(name))
}

/// `/agents` listing for the TUI.
pub fn describe_team() -> String {
    let mut out = String::from("Specialist agents available via the delegate tool:\n");
    for r in all_roles() {
        out.push_str(&format!(
            "  {:<11} {}{}\n",
            r.name,
            r.description,
            if r.read_only { "  (read-only)" } else { "" }
        ));
    }
    out.push_str("\nThe orchestrator decides when to delegate; ask it to \"plan X\", \
                  \"have reviewer check Y\", or \"research Z in parallel\".");
    out
}

// ---------------------------------------------------------------------------
// delegate tool schema + argument parsing
// ---------------------------------------------------------------------------

/// The orchestrator-facing tool definition. One call may spawn several
/// specialists that run concurrently.
pub fn delegate_tool_def(plan_mode: bool) -> ToolDef {
    // In Plan mode only read-only specialists are offered.
    let names: Vec<String> = all_roles()
        .into_iter()
        .filter(|r| !plan_mode || r.read_only)
        .map(|r| r.name)
        .collect();
    ToolDef {
        r#type: "function",
        function: FunctionDef {
            name: "delegate",
            description: "Delegate work to specialist sub-agent(s). Each runs in its own \
                          context with a restricted toolset and reports back. Use for \
                          independent research/planning/coding/testing chunks — up to 4 at \
                          once. Their final reports arrive as your tool result.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": {
                                    "type": "string",
                                    "enum": names,
                                    "description": "Specialist to spawn"
                                },
                                "task": {
                                    "type": "string",
                                    "description": "Precise, self-contained instructions"
                                }
                            },
                            "required": ["agent", "task"]
                        }
                    }
                },
                "required": ["tasks"]
            }),
        },
    }
}

#[derive(Debug, Deserialize)]
struct DelegateTaskArgs {
    agent: String,
    task: String,
}

#[derive(Debug, Deserialize)]
struct DelegateArgs {
    tasks: Vec<DelegateTaskArgs>,
}

pub fn parse_delegate_args(arguments: &str) -> anyhow::Result<Vec<(String, String)>> {
    let parsed: DelegateArgs = serde_json::from_str(arguments)
        .map_err(|e| anyhow::anyhow!("invalid delegate arguments: {e}"))?;
    anyhow::ensure!(
        !parsed.tasks.is_empty(),
        "delegate needs at least one task"
    );
    anyhow::ensure!(
        parsed.tasks.len() <= 4,
        "at most 4 concurrent delegates supported"
    );
    let known: Vec<String> = all_roles().into_iter().map(|r| r.name).collect();
    let mut out = Vec::with_capacity(parsed.tasks.len());
    for t in parsed.tasks {
        let spec = find_role(&t.agent)
            .map(|r| r.name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown agent '{}' — available: {}",
                    t.agent,
                    known.join(", ")
                )
            })?;
        anyhow::ensure!(!t.task.trim().is_empty(), "empty task for '{spec}'");
        out.push((spec, t.task));
    }
    Ok(out)
}

/// Filter the standard toolset down to what a role may touch.
pub fn toolset_for(spec: &Role) -> Vec<ToolDef> {
    crate::tools::tool_defs()
        .into_iter()
        .filter(|t| spec.allowed.iter().any(|a| a == t.function.name))
        .collect()
}

// ---------------------------------------------------------------------------
// Sub-agent execution loop
// ---------------------------------------------------------------------------

use crate::agent::{AgentEvent, ApprovalMode, UiSink};
use crate::api::{ChatClient, Message, StreamEvent};

/// Specialists get fewer rounds than the orchestrator.
pub const MAX_SUB_ROUNDS: usize = 10;

fn sub_system_prompt(cwd: &std::path::Path, spec: &Role) -> String {
    format!(
        "You are the '{}' specialist of the Laudacode coding team.\n\
         Working directory: {cwd}\nOS: {os}\n\n{}\n\nRules: stay strictly within your \
         assignment; inspect before editing; keep changes minimal; end with a short \
         report (findings / changes / next steps).",
        spec.name,
        spec.prompt,
        cwd = cwd.display(),
        os = std::env::consts::OS,
    )
}

/// Run one specialist to completion. Events stream through `ui` (already
/// prefixed by the caller's fork) so the main transcript shows sub-activity.
pub async fn run_sub_agent(
    client: &ChatClient,
    model: &str,
    cwd: &std::path::Path,
    mode: ApprovalMode,
    spec_name: &str,
    task: &str,
    mut ui: Box<dyn UiSink>,
) -> String {
    let Some(spec) = find_role(spec_name) else {
        return format!("error: unknown agent '{spec_name}'");
    };
    let messages = vec![
        Message::system(sub_system_prompt(cwd, &spec)),
        Message::user(task.to_string()),
    ];
    match run_loop(client, model, cwd, mode, &spec, messages, &mut ui).await {
        Ok(report) => report,
        Err(e) => format!("[{spec_name} failed] {e:#}"),
    }
}

async fn run_loop(
    client: &ChatClient,
    model: &str,
    cwd: &std::path::Path,
    mode: ApprovalMode,
    spec: &Role,
    mut messages: Vec<Message>,
    ui: &mut Box<dyn UiSink>,
) -> anyhow::Result<String> {
    let tools = toolset_for(spec);
    for _round in 0..MAX_SUB_ROUNDS {
        let turn = client
            .stream_chat(model, &messages, &tools, |ev| match ev {
                StreamEvent::Content(s) => ui.on_event(AgentEvent::Content(s)),
                StreamEvent::Reasoning(s) => ui.on_event(AgentEvent::Reasoning(s)),
                StreamEvent::Usage(u) => ui.on_event(AgentEvent::Usage(u)),
            }, None)
            .await?;
        if turn.tool_calls.is_empty() {
            if turn.content.trim().is_empty() {
                anyhow::bail!("empty reply");
            }
            return Ok(turn.content);
        }
        messages.push(crate::api::Message::assistant_with_tools(
            turn.tool_calls.clone(),
            if turn.content.is_empty() { None } else { Some(turn.content) },
        ));
        for tc in turn.tool_calls {
            // Toolset restriction is enforced again at execution time.
            if !spec.allowed.iter().any(|a| a == &tc.function.name) {
                messages.push(Message::tool_result(
                    &tc.id,
                    format!("blocked: '{}' is outside the {} role", tc.function.name, spec.name),
                ));
                continue;
            }
            let action = crate::tools::parse_tool_action(
                &tc.function.name,
                &tc.function.arguments,
            )
            .map_err(|e| e.context("parsing sub-agent tool call"))?;
            ui.on_event(AgentEvent::ToolStart {
                name: format!("{}/{}", spec.name, tc.function.name),
                summary: action.describe(),
            });
            // Same approval policy as the orchestrator: Safe flows through,
            // Moderate needs a mutating role + non-plan mode, High always
            // prompts (writes outside the workspace, dangerous commands).
            let approved = match action.danger(cwd) {
                crate::tools::Danger::Safe => true,
                crate::tools::Danger::Moderate => {
                    !spec.read_only && mode != ApprovalMode::Suggest
                }
                crate::tools::Danger::High => ui.approve(&action, crate::tools::Danger::High),
            };
            let result = if !approved {
                "User DECLINED this action.".to_string()
            } else {
                match action.perform_with_diff(cwd).await {
                    Ok((out, files)) => {
                        if !files.is_empty() {
                            ui.on_event(AgentEvent::ToolEdit {
                                name: format!("{}/{}", spec.name, tc.function.name),
                                files,
                            });
                        }
                        out
                    }
                    Err(e) => format!("Command failed: {e:#}"),
                }
            };
            ui.on_event(AgentEvent::ToolDone {
                name: format!("{}/{}", spec.name, tc.function.name),
                ok: !result.starts_with("Command failed") && !result.contains("DECLINED") && !result.starts_with("blocked"),
                preview: result.chars().take(160).collect(),
            });
            messages.push(Message::tool_result(&tc.id, result));
        }
    }
    anyhow::bail!("sub-agent hit its round limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique_and_known_tools_only() {
        let mut seen = std::collections::HashSet::new();
        for s in TEAM {
            assert!(seen.insert(s.name), "duplicate spec {}", s.name);
            let known: Vec<&str> =
                crate::tools::tool_defs().iter().map(|t| t.function.name).collect();
            for a in s.allowed {
                assert!(known.contains(a), "{} allows unknown tool {}", s.name, a);
            }
        }
    }

    #[test]
    fn delegate_args_validate_names_counts_and_tasks() {
        let ok = parse_delegate_args(
            r#"{"tasks":[{"agent":"researcher","task":"find callers of foo"},{"agent":"coder","task":"fix it"}]}"#,
        )
        .unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].0, "researcher");

        assert!(get("RESEARCHER").is_some(), "name lookup is case-insensitive");
        assert!(get("nope").is_none());

        let err = parse_delegate_args(r#"{"tasks":[{"agent":"ghost","task":"x"}]}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown agent 'ghost'"), "{err}");

        let empty = parse_delegate_args(r#"{"tasks":[{"agent":"planner","task":"  "}]}"#);
        assert!(empty.is_err());

        let too_many = parse_delegate_args(
            r#"{"tasks":[{"agent":"planner","task":"1"},{"agent":"planner","task":"2"},{"agent":"planner","task":"3"},{"agent":"planner","task":"4"},{"agent":"planner","task":"5"}]}"#,
        );
        assert!(too_many.is_err());
    }

    #[test]
    fn toolsets_are_scoped_to_role() {
        let coder = find_role("coder").unwrap();
        let set = toolset_for(&coder);
        let names: Vec<&str> = set.iter().map(|t| t.function.name).collect();
        assert!(names.contains(&"apply_patch"));
        assert!(!names.contains(&"run_command"), "coder must not run commands");

        let reviewer = find_role("reviewer").unwrap();
        let names: Vec<&str> =
            toolset_for(&reviewer).iter().map(|t| t.function.name).collect();
        assert!(names.contains(&"grep"));
        assert!(!names.contains(&"write_file"), "reviewer is read-only");

        // Plan-mode schema hides mutating specialists from the enum.
        let def = delegate_tool_def(true);
        let raw = serde_json::to_value(&def).unwrap();
        let enums = raw["function"]["parameters"]["properties"]["tasks"]["items"]["properties"]["agent"]["enum"]
            .as_array()
            .unwrap();
        assert!(!enums.iter().any(|v| v == "coder"), "{enums:?}");
        assert!(enums.iter().any(|v| v == "planner"));
    }
}
