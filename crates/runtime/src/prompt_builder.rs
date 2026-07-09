//! Centralized system prompt builder.
//!
//! Assembles a structured, multi-section system prompt from agent context.
//! Replaces the scattered `push_str` prompt injection throughout the codebase
//! with a single, testable, ordered prompt builder.

use types::message::TurnSummary;

/// A hit from the tree memory system for prompt injection.
#[derive(Debug, Clone)]
pub struct TreeMemoryHit {
    pub scope: String,
    pub kind: String,
    pub content: String,
    pub time_range: String,
}

/// A drawer entry from the kv memory system for prompt injection.
#[derive(Debug, Clone)]
pub struct DrawerEntry {
    pub key: String,
    pub value: Vec<String>,
}

/// All the context needed to build a system prompt for an agent.
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    /// Agent name (from manifest).
    pub agent_name: String,
    /// Agent description (from manifest).
    pub agent_description: String,
    /// Base system prompt authored in the agent manifest.
    pub base_system_prompt: String,
    /// Tool names this agent has access to.
    pub granted_tools: Vec<String>,
    /// Recalled memories as (key, content) pairs.
    pub recalled_memories: Vec<(String, String)>,
    /// Tree memory hits from hierarchical memory (source + global trees).
    pub tree_memories: Vec<TreeMemoryHit>,
    /// Skill summary text (from kernel.build_skill_summary()).
    pub skill_summary: String,
    /// Prompt context from prompt-only skills.
    pub skill_prompt_context: String,
    /// MCP server/tool summary text.
    pub mcp_summary: String,
    /// Agent workspace path.
    pub workspace_path: Option<String>,
    /// SOUL.md content (persona).
    pub soul_md: Option<String>,
    /// USER.md content.
    pub user_md: Option<String>,
    /// MEMORY.md content.
    pub memory_md: Option<String>,
    /// Known user name (from agent's own KV namespace).
    pub user_name: Option<String>,
    /// Channel type (telegram, discord, web, etc.).
    pub channel_type: Option<String>,
    /// Whether this agent was spawned as a subagent.
    pub is_subagent: bool,
    /// Whether this agent has autonomous config.
    pub is_autonomous: bool,
    /// AGENTS.md content (behavioral guidance).
    pub agents_md: Option<String>,
    /// BOOTSTRAP.md content (first-run ritual).
    pub bootstrap_md: Option<String>,
    /// Workspace context section (project type, context files).
    pub workspace_context: Option<String>,
    /// IDENTITY.md content (visual identity + personality frontmatter).
    pub identity_md: Option<String>,
    /// HEARTBEAT.md content (autonomous agent checklist).
    pub heartbeat_md: Option<String>,
    /// Peer agents visible to this agent: (name, state, model).
    pub peer_agents: Vec<(String, String, String)>,
    /// Current date/time string for temporal awareness.
    pub current_date: Option<String>,
    /// Sender identity (e.g. WhatsApp phone number, Telegram user ID).
    pub sender_id: Option<String>,
    /// Sender display name.
    pub sender_name: Option<String>,
    /// User profile summary — preferences, habits, and interaction history
    /// between this clone and the current sender.
    pub user_profile_summary: Option<String>,
    /// Whether the current sender is a clone admin (creator or approved admin).
    /// When true, the prompt signals an admin session so the clone accepts
    /// tuning / internal-task instructions. See docs/ADMIN-MECHANISM.md.
    pub is_admin: bool,
    // --- Clone identity files (分身特有) ---
    /// Clone's system_prompt.md — behavioral instructions ("你怎么做事").
    /// Only present for agents loaded from .agx with a workspace system_prompt.md.
    pub clone_system_prompt_md: Option<String>,
    /// Clone's skill catalog — all skills' name + description (short summary).
    /// Scanned from workspace/skills/ at prompt build time.
    pub clone_skills_catalog: Option<String>,
    /// Clone's style samples — extracted speaking patterns from chat history.
    /// Scanned from workspace/style/ at prompt build time.
    pub clone_style_md: Option<String>,
    /// Clone's full skill prompts — workspace skill body.
    /// Injected alongside the catalog so the LLM knows HOW to execute each skill.
    pub clone_skills_prompts: Option<String>,
    /// Clone's knowledge content — compiled truth from knowledge/*.md files.
    /// Unlike memory_md (which is just the index), this contains actual knowledge.
    pub knowledge_content: Option<String>,
    /// Clone's sub-agents — workspace/agents/*.md parsed at prompt build time.
    pub clone_agents_md: Option<String>,
    /// EVOLUTION.md body text (rules only, frontmatter stripped).
    pub evolution_rules_md: Option<String>,
    // --- V3 identity layer (名人分身专属) ---
    /// MENTAL-MODELS.md — detailed mental models with evidence.
    pub mental_models_md: Option<String>,
    /// DECISION-HEURISTICS.md — decision rules with scenarios.
    pub decision_heuristics_md: Option<String>,
    /// EXPRESSION-DNA.md — quantified style, sentence patterns, catchphrases.
    pub expression_dna_md: Option<String>,
    /// TIMELINE.md — biographical timeline + intellectual lineage.
    pub timeline_md: Option<String>,
    /// Auto-matched skill content — injected at high priority when the system
    /// detects a skill matching the user's message before the LLM call.
    pub auto_matched_skill: Option<String>,
    /// L0 turn summaries — recent conversation turns in condensed form.
    pub turn_summaries: Vec<TurnSummary>,
    /// Drawer entries from kv memory — user profile, preferences, entities, events.
    pub drawer_entries: Vec<DrawerEntry>,
    /// Task ID assigned by the initiator (e.g. cron job). When present, the agent
    /// must use this ID as the output directory and in PUBLISH markers, ensuring
    /// file paths and publish paths are always consistent.
    pub task_id: Option<String>,
}

/// Build the complete system prompt from a `PromptContext`.
///
/// Produces an ordered, multi-section prompt. Sections with no content are
/// omitted entirely (no empty headers). Subagent mode skips sections that
/// add unnecessary context overhead.
pub fn build_system_prompt(ctx: &PromptContext) -> String {
    let mut sections: Vec<String> = Vec::with_capacity(12);

    // Section 1 — Agent Identity (always present)
    sections.push(build_identity_section(ctx));

    // Detect clone mode: base_system_prompt is empty + clone files present
    let is_clone = ctx.base_system_prompt.is_empty()
        && (ctx.clone_system_prompt_md.is_some() || ctx.clone_skills_catalog.is_some());

    // Section 1.05 — Auto-Matched Skill (highest priority, system-detected)
    if let Some(ref skill) = ctx.auto_matched_skill {
        if !skill.trim().is_empty() {
            sections.push(format!(
                "## Active Skill (auto-matched)\n\
                 The skill instructions below are already loaded and active. \
                 Follow these instructions directly — do not call skill_load again for this skill.\n\n{}",
                cap_str(skill, 4000)
            ));
        }
    }

    // Section 1.1 — Clone Identity (分身四部分: 人格 → 行为指令 → 技能目录 → 知识索引)
    // Only for agents loaded from .agx with workspace identity files.
    if is_clone {
        // SOUL.md → 人格
        if let Some(ref soul) = ctx.soul_md {
            if !soul.trim().is_empty() {
                sections.push(format!(
                    "## 人格\n体现以下人格和语气。自然地融入对话，不要生硬或格式化。\n\n{}",
                    cap_str(&strip_code_blocks(soul), 2000)
                ));
            }
        }

        // V3 身份层文件 → 心智模型 + 决策启发式 + 表达DNA + 时间线
        // 只要有任何一个身份文件存在就注入
        if ctx.mental_models_md.is_some()
            || ctx.decision_heuristics_md.is_some()
            || ctx.expression_dna_md.is_some()
            || ctx.timeline_md.is_some()
        {
            let mut identity_section = String::from("## 身份层详解\nSOUL.md 中的视角和风格指向这些详解文件。回答时参考对应内容。\n");
            if let Some(ref mm) = ctx.mental_models_md {
                if !mm.trim().is_empty() {
                    identity_section.push_str(&format!("\n### 心智模型\n{}\n", cap_str(mm, 3000)));
                }
            }
            if let Some(ref dh) = ctx.decision_heuristics_md {
                if !dh.trim().is_empty() {
                    identity_section.push_str(&format!("\n### 决策启发式\n{}\n", cap_str(dh, 1500)));
                }
            }
            if let Some(ref ed) = ctx.expression_dna_md {
                if !ed.trim().is_empty() {
                    identity_section.push_str(&format!("\n### 表达DNA\n{}\n", cap_str(ed, 2000)));
                }
            }
            if let Some(ref tl) = ctx.timeline_md {
                if !tl.trim().is_empty() {
                    identity_section.push_str(&format!("\n### 时间线\n{}\n", cap_str(tl, 1500)));
                }
            }
            sections.push(identity_section);
        }

        // system_prompt.md → 行为指令
        if let Some(ref sp) = ctx.clone_system_prompt_md {
            if !sp.trim().is_empty() {
                sections.push(format!("## 行为指令\n{}", cap_str(sp, 4000)));
            }
        }

        // skills/ → 技能目录（name + description，始终注入，很短）
        if let Some(ref catalog) = ctx.clone_skills_catalog {
            if !catalog.trim().is_empty() {
                let section = if ctx.auto_matched_skill.is_some() {
                    format!(
                        "## 技能目录\n当用户的请求匹配某个技能时，使用 skill_load 加载该技能的详细指令。\n如果某个技能已被自动加载（见上方 Active Skill 部分），直接执行，无需再次调用 skill_load。\n\n{}",
                        catalog
                    )
                } else {
                    format!(
                        "## 技能目录\n当用户的请求匹配某个技能时，使用 skill_load 加载该技能的详细指令，然后严格按指令执行。\n\n{}",
                        catalog
                    )
                };
                sections.push(section);
            }
        }

        // MEMORY.md → 知识索引
        if let Some(ref mem) = ctx.memory_md {
            if !mem.trim().is_empty() {
                sections.push(format!("## 知识索引\n{}", cap_str(mem, 1000)));
            }
        }

        // style/ → 风格参考（真实对话中的说话风格模式）
        if let Some(ref style) = ctx.clone_style_md {
            if !style.trim().is_empty() {
                sections.push(format!(
                    "## 风格参考\n以下是从真实对话中提取的说话风格。参考这些风格模式，但以 SOUL.md 中的人格为主。\n\n{}",
                    cap_str(style, 1500)
                ));
            }
        }

        // agents/ → 子代理目录
        if let Some(ref agents) = ctx.clone_agents_md {
            if !agents.trim().is_empty() {
                sections.push(format!(
                    "## 子代理\n你可以将任务委派给以下子代理。每个子代理有独立的指令和工具。\n\n{}",
                    cap_str(agents, 2000)
                ));
            }
        }

        // EVOLUTION.md → 自我进化规则（frontmatter 之后的规则段落）
        if let Some(ref rules) = ctx.evolution_rules_md {
            if !rules.trim().is_empty() {
                sections.push(format!(
                    "## 自我进化规则\n{}\n\n严格遵守以上进化规则。",
                    cap_str(rules, 2000)
                ));
            }
        }

        // System evolution prompt — auto-injected for all clones
        sections.push(EVOLUTION_PROMPT.to_string());
    }

    // Section 1.5 — Current Date/Time (always present when set)
    if let Some(ref date) = ctx.current_date {
        sections.push(format!("## Current Date\nToday is {date}."));
    }

    // Section 1.6 — Task ID (when assigned by initiator, e.g. cron job)
    if let Some(ref tid) = ctx.task_id {
        sections.push(format!(
            "## 任务 ID\n\
             当前任务 ID: {tid}\n\
             文件输出目录: output/{tid}/\n\
             发布标记格式: [PUBLISH:app_id]output/{tid}/正文.html|文章标题|摘要[/PUBLISH]\n\
             规则：\n\
             1. 所有文件写入 output/{tid}/ 目录，PUBLISH 标记路径也用 output/{tid}/ 开头\n\
             2. 不要把任务 ID 或流水线 ID 写入文章内容或文件开头——文章标题是文章的主题，不是任务 ID\n\
             3. Markdown 文件第一行必须是文章的 # 标题（如 # 阿里 banning Claude 分析）"
        ));
    }

    // Section 2 — Tool Call Behavior (skip for subagents)
    if !ctx.is_subagent {
        sections.push(TOOL_CALL_BEHAVIOR.to_string());
    }

    // Section 2.5 — Agent Behavioral Guidelines (skip for subagents)
    if !ctx.is_subagent {
        if let Some(ref agents) = ctx.agents_md {
            if !agents.trim().is_empty() {
                sections.push(cap_str(agents, 2000));
            }
        }
    }

    // Section 3 — Available Tools (always present if tools exist)
    let tools_section = build_tools_section(&ctx.granted_tools);
    if !tools_section.is_empty() {
        sections.push(tools_section);
    }

    // Section 4 — Memory Protocol (always present)
    let mem_section = build_memory_section(&ctx.recalled_memories, &ctx.tree_memories);
    sections.push(mem_section);

    // Section 4.1 — Drawer Memory (profile, preferences, entities, events from kv)
    if !ctx.drawer_entries.is_empty() {
        sections.push(build_drawer_section(&ctx.drawer_entries));
    }

    // Section 4.2 — L0 Turn Summaries (recent conversation turns)
    if !ctx.turn_summaries.is_empty() && !ctx.is_subagent {
        sections.push(build_turn_summaries_section(&ctx.turn_summaries));
    }

    // Section 5 — Skills (only if skills available)
    if !ctx.skill_summary.is_empty() || !ctx.skill_prompt_context.is_empty() {
        sections.push(build_skills_section(
            &ctx.skill_summary,
            &ctx.skill_prompt_context,
        ));
    }

    // Section 6 — MCP Servers (only if summary present)
    if !ctx.mcp_summary.is_empty() {
        sections.push(build_mcp_section(&ctx.mcp_summary));
    }

    // Section 6.5 — Task Planning (only if task_plan tool is available)
    if ctx.granted_tools.contains(&"task_plan".to_string()) {
        sections.push(TASK_PLAN_GUIDE.to_string());
    }

    // Section 7 — Persona / Identity files (skip for subagents)
    // For clones, SOUL.md and MEMORY.md are already in Section 1.1 — skip them here.
    if !ctx.is_subagent {
        let persona = if is_clone {
            // Clone mode: skip soul_md and memory_md (already in clone section)
            build_persona_section(
                ctx.identity_md.as_deref(),
                None, // soul already in clone section
                ctx.user_md.as_deref(),
                None, // memory already in clone section
                ctx.workspace_path.as_deref(),
            )
        } else {
            build_persona_section(
                ctx.identity_md.as_deref(),
                ctx.soul_md.as_deref(),
                ctx.user_md.as_deref(),
                ctx.memory_md.as_deref(),
                ctx.workspace_path.as_deref(),
            )
        };
        if !persona.is_empty() {
            sections.push(persona);
        }
    }

    // Section 7.5 — Heartbeat checklist (only for autonomous agents)
    if !ctx.is_subagent && ctx.is_autonomous {
        if let Some(ref heartbeat) = ctx.heartbeat_md {
            if !heartbeat.trim().is_empty() {
                sections.push(format!(
                    "## Heartbeat Checklist\n{}",
                    cap_str(heartbeat, 1000)
                ));
            }
        }
    }

    // Section 8 — User Personalization (skip for subagents)
    if !ctx.is_subagent {
        sections.push(build_user_section(ctx.user_name.as_deref()));
    }

    // Section 9 — Channel Awareness (skip for subagents)
    if !ctx.is_subagent {
        if let Some(ref channel) = ctx.channel_type {
            sections.push(build_channel_section(channel));
        }
    }

    // Section 9.1 — Sender Identity (skip for subagents)
    if !ctx.is_subagent {
        if let Some(sender_line) =
            build_sender_section(ctx.sender_name.as_deref(), ctx.sender_id.as_deref())
        {
            sections.push(sender_line);
        }

        // Section 9.1.5 — Admin session signal
        // When the sender is a clone admin, tell the clone it may accept tuning
        // and internal-task instructions. Non-admins get no line (default serve).
        if ctx.is_admin {
            sections.push(
                "## 管理员会话\n当前对话者是本分身的管理员。可接受调教（纠偏/改规则）与对内任务指令（如写公众号、内部运营）；对外服务时仍遵守客服规范。".to_string(),
            );
        }

        // Section 9.1.6 — WeChat OA business identity (from 86bus bind-openid)
        // Surfaced only when the 86bus backend identified the service-account
        // user. Absent for non-weixin-oa users or unidentified senders.
        if let Some(ref sid) = ctx.sender_id {
            if let Some(role) = crate::wechat_identity::get(sid) {
                let label = match role.as_str() {
                    "admin" => "管理员（admin）",
                    "carrier_user" => "运营方/车队（carrier_user）",
                    _ => "普通用户",
                };
                sections.push(format!(
                    "## 当前用户身份（86bus 业务系统）\n该用户经识别为：{label}。按身份差异化对待：管理员可接受调教与内部运营指令；运营方关注车队/包车对接；普通用户走标准客服流程。"
                ));
            }
        }

        // Section 9.2 — User Profile (multi-tenancy)
        if let Some(ref profile) = ctx.user_profile_summary {
            sections.push(format!("## User Profile\n{}", profile));
        }
    }

    // Section 9.5 — Peer Agent Awareness (skip for subagents)
    if !ctx.is_subagent && !ctx.peer_agents.is_empty() {
        sections.push(build_peer_agents_section(&ctx.agent_name, &ctx.peer_agents));
    }

    // Section 10 — Safety & Oversight (skip for subagents)
    if !ctx.is_subagent {
        sections.push(SAFETY_SECTION.to_string());
    }

    // Section 11 — Operational Guidelines (always present)
    sections.push(OPERATIONAL_GUIDELINES.to_string());

    // Section 12 — Bootstrap Protocol (only on first-run, skip for subagents)
    if !ctx.is_subagent {
        if let Some(ref bootstrap) = ctx.bootstrap_md {
            if !bootstrap.trim().is_empty() {
                // Only inject if no user_name memory exists (first-run heuristic)
                let has_user_name = ctx.recalled_memories.iter().any(|(k, _)| k == "user_name");
                if !has_user_name && ctx.user_name.is_none() {
                    sections.push(format!(
                        "## First-Run Protocol\n{}",
                        cap_str(bootstrap, 1500)
                    ));
                }
            }
        }
    }

    // Section 14 — Workspace Context (skip for subagents)
    if !ctx.is_subagent {
        if let Some(ref ws_ctx) = ctx.workspace_context {
            if !ws_ctx.trim().is_empty() {
                sections.push(cap_str(ws_ctx, 1000));
            }
        }
    }

    sections.join("\n\n")
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

fn build_identity_section(ctx: &PromptContext) -> String {
    // Clone mode: base_system_prompt is empty, identity built from workspace files
    let is_clone = ctx.base_system_prompt.is_empty()
        && (ctx.clone_system_prompt_md.is_some() || ctx.clone_skills_catalog.is_some());

    if is_clone {
        // For clones, just set the name. The four-part identity
        // (SOUL → system_prompt → skills → MEMORY) is built in separate sections.
        format!("You are {}.\n{}", ctx.agent_name, ctx.agent_description)
    } else if ctx.base_system_prompt.is_empty() {
        format!(
            "You are {}, an AI agent running inside the OpenCarrier Agent OS.\n{}",
            ctx.agent_name, ctx.agent_description
        )
    } else {
        ctx.base_system_prompt.clone()
    }
}

/// Static tool-call behavior directives.
const TOOL_CALL_BEHAVIOR: &str = "\
## Tool Call Behavior
- When you need to use a tool, call it immediately. Do not narrate or explain routine tool calls.
- Only explain tool calls when the action is destructive, unusual, or the user explicitly asked for an explanation.
- Prefer action over narration. If you can answer by using a tool, do it.
- When executing multiple sequential tool calls, batch them — don't output reasoning between each call.
- If a tool returns useful results, present the KEY information, not the raw output.
- When web_fetch or web_search returns content, you MUST include the relevant data in your response. \
Quote specific facts, numbers, or passages from the fetched/searched content. Never say you fetched something \
without sharing what you found.
- Start with the answer, not meta-commentary about how you'll help.
- IMPORTANT: If your instructions or persona mention a shell command, script path, or code snippet, \
execute it via the appropriate tool call (shell_exec, file_write, etc.). Never output commands as \
code blocks — always call the tool instead.";

const TASK_PLAN_GUIDE: &str = "\
## Task Planning
When a task is complex and cannot be completed in a single turn (e.g. \
multi-stage workflows like research → write → format → publish), use \
the `task_plan` tool to split it into steps. Each step runs independently \
with its own iteration budget. Steps without dependencies run in parallel.

**When to use task_plan:**
- The task has 3+ distinct stages
- A single turn would exceed your iteration limit
- Stages can be clearly separated (e.g. gathering info, then acting on it)

**When NOT to use task_plan:**
- Simple tasks that complete in one turn
- Tasks where stages are tightly interleaved

Example:
```json
{\"title\": \"Write and publish article\", \"steps\": [
  {\"id\": \"research\", \"prompt\": \"Search for trending AI topics and select the best one\", \"depends_on\": []},
  {\"id\": \"write\", \"prompt\": \"Write a 1000-word article about the selected topic\", \"depends_on\": [\"research\"]},
  {\"id\": \"publish\", \"prompt\": \"Format the article and publish to WeChat\", \"depends_on\": [\"write\"]}
]}
```";

/// Tools that have been removed and should not be used.
/// If conversation history references these, the agent must NOT attempt to call them.
const REMOVED_TOOLS: &[&str] = &[
    "docker_exec",
    "process_start",
    "process_poll",
    "process_write",
    "process_kill",
    "process_list",
    "knowledge_add_entity",
    "knowledge_add_relation",
    "knowledge_query",
];

/// Build the grouped tools section (Section 3).
pub fn build_tools_section(granted_tools: &[String]) -> String {
    if granted_tools.is_empty() {
        return String::new();
    }

    // Group tools by category
    let mut groups: std::collections::BTreeMap<&str, Vec<(&str, &str)>> =
        std::collections::BTreeMap::new();
    for name in granted_tools {
        let cat = tool_category(name);
        let hint = tool_hint(name);
        groups.entry(cat).or_default().push((name.as_str(), hint));
    }

    let mut out = String::from(
        "## Your Tools\n\
         You have access to these capabilities. This is your starting tool set.\n\
         \n\
         **IMPORTANT**: You can ONLY use tools listed below OR tools discovered via `tool_search`. \
         Do NOT guess or invent tool names. If you need a capability not listed below, you MUST \
         call `tool_search(\"what you need\")` first — it returns the tool's name, description, \
         and parameter schema. Then you can call that tool directly.\n"
    );
    for (category, tools) in &groups {
        out.push_str(&format!("\n**{}**: ", capitalize(category)));
        let descs: Vec<String> = tools
            .iter()
            .map(|(name, hint)| {
                if hint.is_empty() {
                    (*name).to_string()
                } else {
                    format!("{name} ({hint})")
                }
            })
            .collect();
        out.push_str(&descs.join(", "));
    }

    // Warn about removed tools that the agent might reference from old session history
    let removed: Vec<&&str> = REMOVED_TOOLS
        .iter()
        .filter(|t| !granted_tools.iter().any(|g| g == **t))
        .collect();
    if !removed.is_empty() {
        out.push_str("\n\n**Removed tools** (do NOT attempt to call these): ");
        out.push_str(&removed.iter().map(|t| t.as_ref()).collect::<Vec<_>>().join(", "));
        out.push('.');
    }

    out
}

/// Build the memory section (Section 4).
///
/// Also used by `agent_loop.rs` to append recalled memories after DB lookup.
pub fn build_memory_section(memories: &[(String, String)], tree_hits: &[TreeMemoryHit]) -> String {
    let mut out = String::from("## Memory\n");

    // Tree memory hits (hierarchical memory) — prefetched context
    if !tree_hits.is_empty() {
        out.push_str("[Recent context from memory tree]\n");
        for hit in tree_hits.iter().take(5) {
            let capped = cap_str(&hit.content, 500);
            out.push_str(&format!("- [{}/{}] {} ({})\n", hit.kind, hit.scope, capped, hit.time_range));
        }
        out.push('\n');
    }

    // Tool usage guide — memory_tree is available via tool_search when needed
    out.push_str("`memory_tree` is available for querying past conversation/email/document history. Use `tool_search(\"memory_tree\")` to discover it when the user asks about past interactions.\n");

    // Legacy flat memories (if any)
    if !memories.is_empty() {
        out.push_str("\nRecalled memories:\n");
        for (key, content) in memories.iter().take(5) {
            let capped = cap_str(content, 500);
            if key.is_empty() {
                out.push_str(&format!("- {capped}\n"));
            } else {
                out.push_str(&format!("- [{key}] {capped}\n"));
            }
        }
    }

    out
}

/// Build the drawer section — injected kv memory organized by category.
fn build_drawer_section(entries: &[DrawerEntry]) -> String {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<&str, Vec<&DrawerEntry>> = BTreeMap::new();
    for entry in entries {
        let prefix = entry.key.split('.').next().unwrap_or("other");
        groups.entry(prefix).or_default().push(entry);
    }

    let mut out = String::from("## Drawer Memory\n");

    // Render each group with a localized header
    for (prefix, items) in &groups {
        let header = match *prefix {
            "profile" => "User profile",
            "preference" => "Preferences",
            "entity" => "Entities",
            "fact" => "Facts",
            "event" => "Recent events",
            _ => prefix,
        };
        out.push_str(&format!("[{}]\n", header));

        // For event entries, only show last 5
        let items_to_show: Vec<&&DrawerEntry> = if *prefix == "event" {
            items.iter().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect()
        } else {
            items.iter().collect()
        };

        for entry in items_to_show {
            let name = entry.key.split('.').nth(1).unwrap_or(&entry.key);
            let values = entry.value.join(", ");
            out.push_str(&format!("- {}: {}\n", name, values));
        }
        out.push('\n');
    }

    out
}

/// Build the L0 turn summaries section.
fn build_turn_summaries_section(summaries: &[TurnSummary]) -> String {
    let mut out = String::from("## Recent Conversations\n");

    for summary in summaries.iter().take(10) {
        let tools = if summary.tools_used.is_empty() {
            String::new()
        } else {
            format!(" [{}]", summary.tools_used.join(","))
        };
        out.push_str(&format!(
            "- {} → {}{}\n",
            summary.user_intent, summary.assistant_outcome, tools
        ));
    }

    out
}

fn build_skills_section(skill_summary: &str, prompt_context: &str) -> String {
    let mut out = String::from("## Skills\n");
    if !skill_summary.is_empty() {
        out.push_str(
            "You have installed skills. If a request matches a skill, use its tools directly.\n",
        );
        out.push_str(skill_summary.trim());
    }
    if !prompt_context.is_empty() {
        out.push('\n');
        out.push_str(&cap_str(prompt_context, 2000));
    }
    out
}

fn build_mcp_section(mcp_summary: &str) -> String {
    format!("## Connected Tool Servers (MCP)\n{}", mcp_summary.trim())
}

fn build_persona_section(
    identity_md: Option<&str>,
    soul_md: Option<&str>,
    user_md: Option<&str>,
    memory_md: Option<&str>,
    workspace_path: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(ws) = workspace_path {
        parts.push(format!("## Workspace\nWorkspace: {ws}"));
    }

    // Identity file (IDENTITY.md) — personality at a glance, before SOUL.md
    if let Some(identity) = identity_md {
        if !identity.trim().is_empty() {
            parts.push(format!("## Identity\n{}", cap_str(identity, 500)));
        }
    }

    if let Some(soul) = soul_md {
        if !soul.trim().is_empty() {
            let sanitized = strip_code_blocks(soul);
            parts.push(format!(
                "## Persona\nEmbody this identity in your tone and communication style. Be natural, not stiff or generic.\n{}",
                cap_str(&sanitized, 1000)
            ));
        }
    }

    if let Some(user) = user_md {
        if !user.trim().is_empty() {
            parts.push(format!("## User Context\n{}", cap_str(user, 500)));
        }
    }

    if let Some(memory) = memory_md {
        if !memory.trim().is_empty() {
            parts.push(format!("## Long-Term Memory\n{}", cap_str(memory, 500)));
        }
    }

    parts.join("\n\n")
}

fn build_user_section(user_name: Option<&str>) -> String {
    match user_name {
        Some(name) => {
            format!(
                "## User Profile\n\
                 The user's name is \"{name}\". Address them by name naturally \
                 when appropriate (greetings, farewells, etc.), but don't overuse it."
            )
        }
        None => "## User Profile\n\
             You don't know the user's name yet. On your FIRST reply in this conversation, \
             warmly introduce yourself by your agent name and ask what they'd like to be called. \
             Remember their name for future sessions. \
             Keep the introduction brief — don't let it overshadow their actual request."
            .to_string(),
    }
}

fn build_channel_section(channel: &str) -> String {
    let (limit, hints) = match channel {
        "telegram" => (
            "4096",
            "Use Telegram-compatible formatting (bold with *, code with `backticks`).",
        ),
        "discord" => (
            "2000",
            "Use Discord markdown. Split long responses across multiple messages if needed.",
        ),
        "slack" => (
            "4000",
            "Use Slack mrkdwn formatting (*bold*, _italic_, `code`).",
        ),
        "whatsapp" => (
            "4096",
            "Keep messages concise. WhatsApp has limited formatting.",
        ),
        "irc" => (
            "512",
            "Keep messages very short. No markdown — plain text only.",
        ),
        "matrix" => (
            "65535",
            "Matrix supports rich formatting. Use markdown freely.",
        ),
        "teams" => ("28000", "Use Teams-compatible markdown."),
        _ => ("4096", "Use markdown formatting where supported."),
    };
    format!(
        "## Channel\n\
         You are responding via {channel}. Keep messages under {limit} chars.\n\
         {hints}"
    )
}

fn build_sender_section(sender_name: Option<&str>, sender_id: Option<&str>) -> Option<String> {
    match (sender_name, sender_id) {
        (Some(name), Some(id)) => Some(format!("## Sender\nMessage from: {name} ({id})")),
        (Some(name), None) => Some(format!("## Sender\nMessage from: {name}")),
        (None, Some(id)) => Some(format!("## Sender\nMessage from: {id}")),
        (None, None) => None,
    }
}

fn build_peer_agents_section(self_name: &str, peers: &[(String, String, String)]) -> String {
    let mut out = String::from(
        "## Peer Agents\n\
         You are part of a multi-agent system. These agents are running alongside you:\n",
    );
    for (name, state, model) in peers {
        if name == self_name {
            continue; // Don't list yourself
        }
        out.push_str(&format!("- **{}** ({}) — model: {}\n", name, state, model));
    }
    out.push_str(
        "\nYou can communicate with them using `agent_send` (by name) and see all agents with `agent_list`. \
         Delegate tasks to specialized agents when appropriate.",
    );
    out
}

/// Static safety section.
const SAFETY_SECTION: &str = "\
## Safety
- Prioritize safety and human oversight over task completion.
- NEVER auto-execute purchases, payments, account deletions, or irreversible actions without explicit user confirmation.
- If a tool could cause data loss, explain what it will do and confirm first.
- If you cannot accomplish a task safely, explain the limitation.
- When in doubt, ask the user.";

/// System evolution prompt — auto-injected for all clones.
/// Tells the clone about self-improvement tools so it can learn and adapt autonomously.
const EVOLUTION_PROMPT: &str = "\
## 自我进化
你拥有自我学习和改进的能力。在对话中发现有价值的新知识时，主动使用工具保存：
- **kv_set**: 存储用户的重要信息到抽屉（账号、偏好、决策等）
- **kv_get**: 在提问前先检查抽屉，避免重复询问用户已提供的信息
- **knowledge_extract**: 从对话中提取新知识（事实、规则、偏好）并保存到知识库
- **skill_create**: 创建新技能来扩展你的能力
- **skill_update**: 改进现有技能的流程和内容（如：发现缺少信息时，更新 skill 写入经验）
- **session_summarize**: 总结长对话的关键信息以备后续回忆

重要学习模式：当你发现执行某个任务缺少关键信息（如公众号名称、API密钥等），
向用户获取后成功完成任务，你应该：
1. kv_set 存储这个信息（下次不用再问）
2. skill_update 在对应的技能中写入经验（如：发公众号前先 kv_get(entity.wechat_accounts)）

不需要每次对话都调用，只在有实质性新知识或改进机会时使用。越用越好。";

/// Static operational guidelines (replaces STABILITY_GUIDELINES).
const OPERATIONAL_GUIDELINES: &str = "\
## Operational Guidelines
- Do NOT retry a tool call with identical parameters if it failed. Try a different approach.
- If a tool returns an error, analyze the error before calling it again.
- Prefer targeted, specific tool calls over broad ones.
- Plan your approach before executing multiple tool calls.
- If you cannot accomplish a task after a few attempts, explain what went wrong instead of looping.
- Never call the same tool more than 3 times with the same parameters.
- If a message requires no response (simple acknowledgments, reactions, messages not directed at you), respond with exactly NO_REPLY.";

// ---------------------------------------------------------------------------
// Tool metadata helpers
// ---------------------------------------------------------------------------

/// Map a tool name to its category for grouping.
pub fn tool_category(name: &str) -> &'static str {
    match name {
        "file_read" | "file_write" | "file_list" | "file_delete" | "file_move" | "file_copy"
        | "file_search" => "Files",

        "web_fetch" => "Web",

        "browser_navigate" | "browser_click" | "browser_type" | "browser_screenshot"
        | "browser_read_page" | "browser_close" | "browser_scroll" | "browser_wait"
        | "browser_evaluate" | "browser_select" | "browser_back" => "Browser",

        "shell_exec" | "shell_background" => "Shell",

        "agent_send" | "agent_spawn" | "agent_list" | "agent_kill" => "Agents",

        "image_describe" | "image_generate" | "audio_transcribe" | "tts_speak" => "Media",

        "cron_create" | "cron_list" | "cron_delete" => "Scheduling",

        "process_start" | "process_poll" | "process_write" | "process_kill" | "process_list" => {
            "Processes"
        }

        _ if name.starts_with("mcp_") => "MCP",
        _ if name.starts_with("skill_") => "Skills",
        _ => "Other",
    }
}

/// Map a tool name to a one-line description hint.
pub fn tool_hint(name: &str) -> &'static str {
    match name {
        // Files
        "file_read" => "read file contents",
        "file_write" => "create or overwrite a file",
        "file_list" => "list directory contents",
        "file_delete" => "delete a file",
        "file_move" => "move or rename a file",
        "file_copy" => "copy a file",
        "file_search" => "search files by name pattern",

        // Web
        "web_fetch" => "fetch a URL and get its content as markdown",

        // Browser
        "browser_navigate" => "open a URL in the browser and return content",
        "browser_click" => "click an element on the page via JS",
        "browser_type" => "type text into an input field via JS",
        "browser_screenshot" => "capture a screenshot (not supported — use browser_navigate)",
        "browser_read_page" => "extract page content as text/markdown",
        "browser_close" => "close the browser session (no-op for AginxBrowser)",
        "browser_scroll" => "scroll the page via JS",
        "browser_wait" => "wait for an element or condition via JS",
        "browser_evaluate" => "run arbitrary JavaScript on the page",
        "browser_select" => "select a dropdown option via JS",
        "browser_back" => "go back to the previous page (not supported — use browser_navigate)",

        // Shell
        "shell_exec" => "execute a shell command",
        "shell_background" => "run a command in the background",

        // Agents
        "agent_send" => "send a message to another agent",
        "agent_spawn" => "create a new agent",
        "agent_list" => "list running agents",
        "agent_kill" => "terminate an agent",

        // Media
        "image_describe" => "describe an image",
        "image_generate" => "generate an image from a prompt",
        "audio_transcribe" => "transcribe audio to text",
        "tts_speak" => "convert text to speech",

        // Scheduling
        "cron_create" => "schedule a recurring task",
        "cron_list" => "list scheduled tasks",
        "cron_delete" => "remove a scheduled task",

        // Processes
        "process_start" => "start a long-running process (REPL, server)",
        "process_poll" => "read stdout/stderr from a running process",
        "process_write" => "write to a process's stdin",
        "process_kill" => "terminate a running process",
        "process_list" => "list active processes",

        // Evolution (self-improvement)
        "knowledge_extract" => "extract and save new knowledge from conversation",
        "knowledge_index" => "rebuild knowledge index (MEMORY.md)",
        "skill_create" => "create a new skill",
        "skill_update" => "update an existing skill",
        "skill_load" => "load full skill content",
        "session_summarize" => "save a conversation summary",

        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Cap a string to `max_chars`, appending "..." if truncated.
/// Strip markdown triple-backtick code blocks from content.
///
/// Prevents LLMs from copying code blocks as text output instead of making
/// tool calls when SOUL.md contains command examples.
fn strip_code_blocks(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut in_block = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if !in_block {
            result.push_str(line);
            result.push('\n');
        }
    }
    // Collapse multiple blank lines left by stripped blocks
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

fn cap_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}...", &s[..end])
    }
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_ctx() -> PromptContext {
        PromptContext {
            agent_name: "researcher".to_string(),
            agent_description: "Research agent".to_string(),
            base_system_prompt: "You are Researcher, a research agent.".to_string(),
            granted_tools: vec![
                "web_fetch".to_string(),
                "web_fetch".to_string(),
                "file_read".to_string(),
                "file_write".to_string(),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_full_prompt_has_all_sections() {
        let prompt = build_system_prompt(&basic_ctx());
        assert!(prompt.contains("You are Researcher"));
        assert!(prompt.contains("## Tool Call Behavior"));
        assert!(prompt.contains("## Your Tools"));
        assert!(prompt.contains("## Memory"));
        assert!(prompt.contains("## User Profile"));
        assert!(prompt.contains("## Safety"));
        assert!(prompt.contains("## Operational Guidelines"));
    }

    #[test]
    fn test_section_ordering() {
        let prompt = build_system_prompt(&basic_ctx());
        let tool_behavior_pos = prompt.find("## Tool Call Behavior").unwrap();
        let tools_pos = prompt.find("## Your Tools").unwrap();
        let memory_pos = prompt.find("## Memory").unwrap();
        let safety_pos = prompt.find("## Safety").unwrap();
        let guidelines_pos = prompt.find("## Operational Guidelines").unwrap();

        assert!(tool_behavior_pos < tools_pos);
        assert!(tools_pos < memory_pos);
        assert!(memory_pos < safety_pos);
        assert!(safety_pos < guidelines_pos);
    }

    #[test]
    fn test_subagent_omits_sections() {
        let mut ctx = basic_ctx();
        ctx.is_subagent = true;
        let prompt = build_system_prompt(&ctx);

        assert!(!prompt.contains("## Tool Call Behavior"));
        assert!(!prompt.contains("## User Profile"));
        assert!(!prompt.contains("## Channel"));
        assert!(!prompt.contains("## Safety"));
        // Subagents still get tools and guidelines
        assert!(prompt.contains("## Your Tools"));
        assert!(prompt.contains("## Operational Guidelines"));
        assert!(prompt.contains("## Memory"));
    }

    #[test]
    fn test_empty_tools_no_section() {
        let ctx = PromptContext {
            agent_name: "test".to_string(),
            ..Default::default()
        };
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("## Your Tools"));
    }

    #[test]
    fn test_tool_grouping() {
        let tools = vec![
            "web_fetch".to_string(),
            "web_fetch".to_string(),
            "file_read".to_string(),
            "browser_navigate".to_string(),
        ];
        let section = build_tools_section(&tools);
        assert!(section.contains("**Browser**"));
        assert!(section.contains("**Files**"));
        assert!(section.contains("**Web**"));
    }

    #[test]
    fn test_tool_categories() {
        assert_eq!(tool_category("file_read"), "Files");
        assert_eq!(tool_category("web_fetch"), "Web");
        assert_eq!(tool_category("browser_navigate"), "Browser");
        assert_eq!(tool_category("shell_exec"), "Shell");
        assert_eq!(tool_category("agent_send"), "Agents");
        assert_eq!(tool_category("mcp_github_search"), "MCP");
        assert_eq!(tool_category("unknown_tool"), "Other");
    }

    #[test]
    fn test_tool_hints() {
        assert!(!tool_hint("web_fetch").is_empty());
        assert!(!tool_hint("file_read").is_empty());
        assert!(!tool_hint("browser_navigate").is_empty());
        assert!(tool_hint("some_unknown_tool").is_empty());
    }

    #[test]
    fn test_memory_section_empty() {
        let section = build_memory_section(&[], &[]);
        assert!(section.contains("## Memory"));
        assert!(!section.contains("Recalled memories"));
    }

    #[test]
    fn test_memory_section_with_items() {
        let memories = vec![
            ("pref".to_string(), "User likes dark mode".to_string()),
            ("ctx".to_string(), "Working on Rust project".to_string()),
        ];
        let section = build_memory_section(&memories, &[]);
        assert!(section.contains("Recalled memories"));
        assert!(section.contains("[pref] User likes dark mode"));
        assert!(section.contains("[ctx] Working on Rust project"));
    }

    #[test]
    fn test_memory_cap_at_5() {
        let memories: Vec<(String, String)> = (0..10)
            .map(|i| (format!("k{i}"), format!("value {i}")))
            .collect();
        let section = build_memory_section(&memories, &[]);
        assert!(section.contains("[k0]"));
        assert!(section.contains("[k4]"));
        assert!(!section.contains("[k5]"));
    }

    #[test]
    fn test_memory_content_capped() {
        let long_content = "x".repeat(1000);
        let memories = vec![("k".to_string(), long_content)];
        let section = build_memory_section(&memories, &[]);
        // Should be capped at 500 + "..."
        assert!(section.contains("..."));
        assert!(section.len() < 1600);
    }

    #[test]
    fn test_skills_section_omitted_when_empty() {
        let ctx = basic_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("## Skills"));
    }

    #[test]
    fn test_skills_section_present() {
        let mut ctx = basic_ctx();
        ctx.skill_summary = "- web-search: Search the web\n- git-expert: Git commands".to_string();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("## Skills"));
        assert!(prompt.contains("web-search"));
    }

    #[test]
    fn test_mcp_section_omitted_when_empty() {
        let ctx = basic_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("## Connected Tool Servers"));
    }

    #[test]
    fn test_mcp_section_present() {
        let mut ctx = basic_ctx();
        ctx.mcp_summary = "- github: 5 tools (search, create_issue, ...)".to_string();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("## Connected Tool Servers (MCP)"));
        assert!(prompt.contains("github"));
    }

    #[test]
    fn test_persona_section_with_soul() {
        let mut ctx = basic_ctx();
        ctx.soul_md = Some("You are a pirate. Arr!".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("## Persona"));
        assert!(prompt.contains("pirate"));
    }

    #[test]
    fn test_persona_soul_capped_at_1000() {
        let long_soul = "x".repeat(2000);
        let section = build_persona_section(None, Some(&long_soul), None, None, None);
        assert!(section.contains("..."));
        // The raw soul content in the section should be at most 1003 chars (1000 + "...")
        assert!(section.len() < 1200);
    }

    #[test]
    fn test_channel_telegram() {
        let section = build_channel_section("telegram");
        assert!(section.contains("4096"));
        assert!(section.contains("Telegram"));
    }

    #[test]
    fn test_channel_discord() {
        let section = build_channel_section("discord");
        assert!(section.contains("2000"));
        assert!(section.contains("Discord"));
    }

    #[test]
    fn test_channel_irc() {
        let section = build_channel_section("irc");
        assert!(section.contains("512"));
        assert!(section.contains("plain text"));
    }

    #[test]
    fn test_channel_unknown_gets_default() {
        let section = build_channel_section("smoke_signal");
        assert!(section.contains("4096"));
        assert!(section.contains("smoke_signal"));
    }

    #[test]
    fn test_user_name_known() {
        let mut ctx = basic_ctx();
        ctx.user_name = Some("Alice".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("Alice"));
        assert!(!prompt.contains("don't know the user's name"));
    }

    #[test]
    fn test_admin_session_signal() {
        // Non-admin (default): no admin section
        let prompt = build_system_prompt(&basic_ctx());
        assert!(!prompt.contains("管理员会话"));

        // Admin: section present
        let mut ctx = basic_ctx();
        ctx.is_admin = true;
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("## 管理员会话"));
        assert!(prompt.contains("管理员"));
    }

    #[test]
    fn test_user_name_unknown() {
        let ctx = basic_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("don't know the user's name"));
    }

    #[test]
    fn test_empty_base_prompt_generates_default_identity() {
        let ctx = PromptContext {
            agent_name: "helper".to_string(),
            agent_description: "A helpful agent".to_string(),
            ..Default::default()
        };
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("You are helper"));
        assert!(prompt.contains("A helpful agent"));
    }

    #[test]
    fn test_workspace_in_persona() {
        let mut ctx = basic_ctx();
        ctx.workspace_path = Some("/home/user/project".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("## Workspace"));
        assert!(prompt.contains("/home/user/project"));
    }

    #[test]
    fn test_cap_str_short() {
        assert_eq!(cap_str("hello", 10), "hello");
    }

    #[test]
    fn test_cap_str_long() {
        let result = cap_str("hello world", 5);
        assert_eq!(result, "hello...");
    }

    #[test]
    fn test_cap_str_multibyte_utf8() {
        // This was panicking with "byte index is not a char boundary" (#38)
        let chinese = "你好世界这是一个测试字符串";
        let result = cap_str(chinese, 4);
        assert_eq!(result, "你好世界...");
        // Exact boundary
        assert_eq!(cap_str(chinese, 100), chinese);
    }

    #[test]
    fn test_cap_str_emoji() {
        let emoji = "👋🌍🚀✨💯";
        let result = cap_str(emoji, 3);
        assert_eq!(result, "👋🌍🚀...");
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("files"), "Files");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("MCP"), "MCP");
    }

    #[test]
    fn test_clone_mode_sections() {
        let ctx = PromptContext {
            agent_name: "customer-support".to_string(),
            agent_description: "Customer support clone".to_string(),
            base_system_prompt: String::new(), // empty = clone mode
            soul_md: Some("你是专业客服，语气亲切。".to_string()),
            clone_system_prompt_md: Some("处理客户问题，按步骤操作。".to_string()),
            clone_skills_catalog: Some("1. **handle-refund** — 用户要求退货时激活\n2. **handle-complaint** — 用户投诉时激活".to_string()),
            memory_md: Some("## 退货政策\n- [refund-policy](knowledge/refund.md)".to_string()),
            granted_tools: vec!["web_fetch".to_string()],
            ..Default::default()
        };
        let prompt = build_system_prompt(&ctx);

        // Clone sections present
        assert!(prompt.contains("## 人格"));
        assert!(prompt.contains("专业客服"));
        assert!(prompt.contains("## 行为指令"));
        assert!(prompt.contains("处理客户问题"));
        assert!(prompt.contains("## 技能目录"));
        assert!(prompt.contains("handle-refund"));
        assert!(prompt.contains("## 知识索引"));
        assert!(prompt.contains("退货政策"));

        // Persona section should NOT contain soul or memory (already in clone sections)
        let persona_start = prompt.find("## Persona").or(prompt.find("## Workspace"));
        if let Some(start) = persona_start {
            let persona_section = &prompt[start..];
            assert!(!persona_section.contains("专业客服"));
        }
    }

    #[test]
    fn test_clone_mode_no_double_soul() {
        let ctx = PromptContext {
            agent_name: "test-clone".to_string(),
            base_system_prompt: String::new(),
            soul_md: Some("我是测试分身".to_string()),
            clone_system_prompt_md: Some("做一些测试工作".to_string()),
            granted_tools: vec![],
            ..Default::default()
        };
        let prompt = build_system_prompt(&ctx);
        // SOUL should appear exactly once (in clone section, not in persona)
        let count = prompt.matches("我是测试分身").count();
        assert_eq!(
            count, 1,
            "SOUL content should appear exactly once, found {count} times"
        );
    }

    #[test]
    fn test_regular_agent_unchanged_with_clone_fields() {
        // Regular agent with non-empty base_system_prompt should NOT enter clone mode
        let ctx = PromptContext {
            agent_name: "researcher".to_string(),
            agent_description: "Research agent".to_string(),
            base_system_prompt: "You are a researcher.".to_string(),
            clone_system_prompt_md: Some("This should be ignored".to_string()),
            clone_skills_catalog: Some("This too".to_string()),
            granted_tools: vec!["web_fetch".to_string()],
            ..Default::default()
        };
        let prompt = build_system_prompt(&ctx);
        // Should use base_system_prompt, not clone sections
        assert!(prompt.contains("You are a researcher."));
        assert!(!prompt.contains("## 人格"));
        assert!(!prompt.contains("This should be ignored"));
    }
}
