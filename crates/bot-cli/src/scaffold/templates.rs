//! Synthetic starter templates for `bot init` and `bot add` (`__ID__` and `__UID__` placeholders).
//! Profile IDs are always quoted in YAML so valid names like `null` or `true` remain strings.

use crate::cli::Template;

pub type Entry = (&'static str, &'static str);

pub fn files(template: Template) -> &'static [Entry] {
    match template {
        Template::Personal => PERSONAL,
        Template::Team => TEAM,
        Template::Company => COMPANY,
    }
}

pub fn agent_count(template: Template) -> usize {
    files(template)
        .iter()
        .filter(|(path, _)| path.ends_with("/AGENT.md"))
        .count()
}

pub const HOST_JSON: &str = r##"{
  "version": 1,
  "gateway": {"cli": {"accounts": {"owner": {"uid": __UID__}}}},
  "models": {"__ID__": ["assistant"]},
  "providers": {"assistant": {"kind": "demo"}}
}
"##;

pub fn agent_md(leaf: &str) -> String {
    format!(
        "---\ndescription: {leaf} — describe this agent's responsibility in one line\n---\n\
         You are the team's {leaf} agent. Define your responsibilities below.\n\n\
         ## Responsibilities\n- Answer requests within your area of ownership.\n\n\
         ## Out of scope\n- Return requests outside your area to the parent agent instead of answering directly.\n- Never claim to have performed actions that require tools or permissions you do not have.\n"
    )
}

pub fn skill_md(name: &str) -> String {
    if name == "skill-creator" {
        return SKILL_CREATOR.to_owned();
    }
    format!(
        "---\nname: \"{name}\"\ndescription: Describe when to use this skill in one line\n---\n\
         # {name}\n\nSummarize what this skill does and when to trigger it in 2-3 sentences.\n\n\
         ## Steps\n1. What to check first\n2. What to do next\n3. What to include in the reply\n"
    )
}

/// Super-admin skill for creating, updating, and deleting skills or MCP servers (`bot add skill skill-creator`).
pub const SKILL_CREATOR: &str = r##"---
name: "skill-creator"
description: "Create, update, or delete agent skills and add or remove MCP servers with immediate live activation for super-admins. Use on requests like 'create a skill', 'edit/delete a skill', or 'add/remove an MCP server'."
---
# skill-creator

Design and apply profile skill and MCP server changes from a super-admin's natural-language request, then activate them immediately.

## 1. Verify Authorization
- Only principals listed in the profile's `super_admins` may perform these operations.
- If management tools (`skill.inspect`, `skill.create`, `skill.update`, `skill.delete`, `mcp.add`, `mcp.remove`) are unavailable or denied, state clearly that the requester lacks permission and stop.

## 2. Inspect Current State First
- Before creating or modifying a skill, always call `skill.inspect({})` to review agents (`id`, `description`), existing skill names for collisions, per-agent skill size usage (64 KiB limit), registered MCP servers, secret backend (`sops`, `keychain`, `env`), and secret **names**.
- Before updating an existing skill, read its current body with `skill.inspect({"skill": "<name>"})`.

## 3. Choose the Target Agent
- If the requester names an agent, target that agent.
- Otherwise compare agent descriptions from `skill.inspect` and choose the best-matching agent.
- Put a skill at profile scope (`agent: null`) only for rules or formats shared across every agent.

## 4. Write Concise, High-Signal Skills
- **Name (`name`)**: lowercase kebab-case (e.g., `pr-review`, `weekly-summary`).
- **Description (`description`)**: 1-2 concrete sentences stating when to trigger and what it does (avoid vague summaries).
- **Body (`body`)**:
  - Omit generic background the model already knows; write concise imperative rules covering steps, output format, constraints, and edge cases that actually change decisions.
  - Do not include YAML front matter (`---`) in `body` (`skill.create` and `skill.update` generate it automatically).
  - Do not use `allowed-tools`, and keep total skill bytes per agent within the 64 KiB limit.
- Validation and live activation run automatically so changes take effect on the next request.

## 5. Adding/Removing MCP Servers and Encrypted Secrets (SOPS)
- **Remote Streamable HTTP MCP**: call `mcp.add(transport: "http", alias, url, ...)` to fetch `tools/list`, write `tools.lock.yaml`, `tools.yaml`, `SKILL.md`, `host.json`, and `profile.yaml`, and activate immediately.
- **Local stdio MCP**: register with `mcp.add(transport: "stdio", alias, root, command, stdio_tools, ...)`.
- **Risk class (`tool_classes`, `default_class`)**: mark read-only tools `"read"`, irreversible actions (deletes, sends) `"irreversible"`, and other mutations `"write"`.
- **Secret and SOPS rules**:
  - Never write secret **values** (tokens, API keys) into `SKILL.md`, `tools.yaml`, `tools.lock.yaml`, `profile.yaml`, `host.json`, or replies.
  - When a secret already exists in SOPS (`BOT_SECRETS=sops:<file>`) or Keychain, reference only its name (e.g., `auth_secret: "<secret-name>"`); SOPS caches invalidate automatically.
  - Pass `secret_value` (or `secret_values`) only when the super-admin explicitly supplies a new value to store under `bot.<profile>.<name>`, and never echo the value back.
- **Removing MCP servers**: call `mcp.remove(alias, ...)`. Pass `remove_secrets: true` only when the requester explicitly asks to delete unused secrets from the store.

## 6. Report Completion
- Concisely report target scope (agent ID or profile-wide), created/updated/deleted skill or tool names, new revision (first 12 chars), and confirmation that it is live.
"##;

const RULES: &str = r##"- Keep replies concise and direct. Lead with the conclusion, followed by evidence and next steps.
- Say when you do not know something. Never present unverified claims as fact.
- Never copy personal data (names, contacts, accounts) or secret values (tokens, passwords) into replies.
- Before irreversible actions (deletes, deploys, account changes), state the target and impact and wait for human approval.
"##;

const REPORT_FORMAT: &str = r##"---
name: report-format
description: "Structure reports in order: conclusion, evidence, next actions, and unknowns"
---
# Report format

Skills placed under profile `skills/` are visible to all agents so every agent reports consistently.

1. **Conclusion**: one or two sentences.
2. **Evidence**: bulleted sources such as files or documents.
3. **Next actions**: who does what and by when; write "None" if none.
4. **Unknowns**: separate assumptions from verified facts.

Keep this order when combining reports from sub-agents.
"##;

// ── personal ──────────────────────────────────────────────────────────────────────────────────

const PERSONAL: &[Entry] = &[
    ("profile.yaml", PERSONAL_PROFILE),
    ("rules.md", RULES),
    ("agents/main/AGENT.md", PERSONAL_MAIN),
];

const PERSONAL_PROFILE: &str = r##"# Personal assistant profile: minimal starter. See docs/profile-spec.md for all fields.
version: 1
id: "__ID__"                  # Must match the folder name (lowercase, digits, hyphens)
timezone: UTC                 # IANA timezone used by cron triggers
model: assistant              # Model alias allowed for this profile in host.json
roles:
  owner: [run, memory.search, memory.save, memory.forget]   # Role = set of scopes; `run` allows chatting, `memory.*` allows private memory in DM/CLI
members:
  - {identity: "local:owner", roles: [owner]}   # CLI account `owner` mapped to this OS user in host.json
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}   # Terminal entrance -> main agent
secrets: []                   # Declare secret names only; values live in the host secret store
# Optionally tighten authority ceilings via `policy` (defaults to union of role scopes):
# policy:
#   scopes: [run]
#   agents:
#     main/research: [run]
#   approve: []
"##;

const PERSONAL_MAIN: &str = r##"---
description: Personal assistant helping with daily tasks
---
You are the user's personal assistant. Restate the goal in one sentence when needed and answer directly when you can.

## Responsibilities
- Give concise answers about tasks, notes, and plans.
- Ask clarifying questions instead of guessing when details are missing.

## Out of scope
- Never claim to have performed actions without the required tools or permissions.
"##;

// ── team ──────────────────────────────────────────────────────────────────────────────────────

const TEAM: &[Entry] = &[
    ("profile.yaml", TEAM_PROFILE),
    ("rules.md", RULES),
    ("knowledge/team-intro.md", TEAM_INTRO),
    ("skills/report-format/SKILL.md", REPORT_FORMAT),
    ("agents/main/AGENT.md", TEAM_MAIN),
    ("agents/main/skills/delegation/SKILL.md", TEAM_DELEGATION),
    ("agents/main/agents/developer/AGENT.md", TEAM_DEVELOPER),
    ("agents/main/agents/designer/AGENT.md", TEAM_DESIGNER),
    ("agents/main/agents/pm/AGENT.md", TEAM_PM),
];

const TEAM_PROFILE: &str = r##"# Team profile: team lead (main) delegating to developer, designer, and pm agents.
version: 1
id: "__ID__"                  # Must match the folder name (lowercase, digits, hyphens)
timezone: Asia/Seoul          # IANA timezone used by cron triggers
model: assistant              # Model alias allowed for this profile in host.json
roles:
  lead: [run, memory.search, memory.save, memory.forget]   # Role = set of scopes; `run` allows chatting, `memory.*` allows private memory in DM/CLI
members:
  - {identity: "local:owner", roles: [lead]}   # CLI account `owner` mapped to this OS user in host.json
  # Add teammates by Slack identity (register workspace alias in host.json):
  # - {identity: "slack:<workspace-alias>/<user-id>", roles: [lead]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}   # Humans talk only to main (team lead)
secrets: []                   # Declare secret names only; values live in the host secret store
policy:
  scopes: [run, memory.search, memory.save, memory.forget]   # Profile-wide ceiling
  agents:                     # Per-agent ceilings (omitted agents inherit their parent ceiling)
    main/developer: [run]
    main/designer: [run]
    main/pm: [run]
  approve: []                 # Write scopes requiring approval (irreversible tools always require approval)
"##;

const TEAM_INTRO: &str = r##"# Team Overview (Example)

Replace this file with your team overview. Agents cite `knowledge/` when answering team questions.

- Team name: Example Team
- Mission: Build and operate the product.
- Workflow: The team lead receives requests and delegates specialist work to sub-agents.
"##;

const TEAM_MAIN: &str = r##"---
description: Team lead who triages requests, delegates to sub-agents, and synthesizes answers
---
You are the team lead. Answer simple requests directly; delegate specialist work to sub-agents and combine their results into one clear reply.

## Responsibilities
- Restate the request in one sentence and decide what to delegate (`delegation` skill).
- Do not paste sub-agent outputs verbatim. Synthesize using `report-format` (conclusion, evidence, next actions, unknowns) and attribute each agent's input.
- Answer team overview questions from `knowledge/` with citations.

## Out of scope
- Do not guess on code, design, or product specs; delegate or state what is unknown.
- Do not assign sub-agents work that exceeds their permitted scopes.
"##;

const TEAM_DELEGATION: &str = r##"---
name: delegation
description: Decide which direct sub-agent should handle a request based on its domain
---
# Delegation Guide

Assign each focused request to one sub-agent, or split cross-functional work across domains. You may only delegate to direct children.

| Request type | Sub-agent |
|---|---|
| Code explanation, code review, root-cause analysis | `main/developer` |
| UI/UX flow review, copy review, design principles | `main/designer` |
| Requirements, scope, timeline, prioritization | `main/pm` |

## When delegating
- Include goal, context, and desired output shape (table, list, paragraph) in one prompt; sub-agents do not see the full conversation.
- Check `policy.agents` in `profile.yaml` before asking a sub-agent to perform sensitive work.

## After receiving results
- Synthesize into conclusion, evidence, and next actions with attribution rather than forwarding raw text.
- Surface disagreements explicitly instead of hiding conflicting conclusions.
"##;

const TEAM_DEVELOPER: &str = r##"---
description: Developer handling code explanation, review, and bug root-cause analysis
---
You are the team's developer. Answer code and architecture questions with concrete evidence.

## Responsibilities
- Handle code explanations, code reviews, and root-cause analysis.
- Label unverified assumptions clearly.

## Out of scope
- Do not run or deploy code (this agent has no tools or permissions to do so).
"##;

const TEAM_DESIGNER: &str = r##"---
description: Designer handling UI flow, copy review, and design principles
---
You are the team's designer. Review screens, flows, and copy from the user's perspective.

## Responsibilities
- Review UI flows and copy, explaining the rationale for every suggested change.
- Check accessibility (contrast, legibility, keyboard navigation).

## Out of scope
- Do not assert subjective taste without rationale.
"##;

const TEAM_PM: &str = r##"---
description: Product manager structuring requirements, timelines, and priorities
---
You are the team's product manager. Clarify requirements and propose priorities.

## Responsibilities
- Break requests into goals, scope, and acceptance criteria.
- Propose priorities with rationale and leave final decisions to humans.

## Out of scope
- Do not commit to firm deadlines; provide grounded estimates only.
"##;

// ── company ───────────────────────────────────────────────────────────────────────────────────

const COMPANY: &[Entry] = &[
    ("profile.yaml", COMPANY_PROFILE),
    ("rules.md", RULES),
    ("knowledge/company-intro.md", COMPANY_INTRO),
    ("skills/report-format/SKILL.md", REPORT_FORMAT),
    ("agents/main/AGENT.md", COMPANY_MAIN),
    ("agents/main/skills/delegation/SKILL.md", COMPANY_DELEGATION),
    ("agents/main/agents/product/AGENT.md", COMPANY_PRODUCT),
    (
        "agents/main/agents/product/agents/engineer/AGENT.md",
        COMPANY_ENGINEER,
    ),
    (
        "agents/main/agents/product/agents/designer/AGENT.md",
        COMPANY_DESIGNER,
    ),
    ("agents/main/agents/support/AGENT.md", COMPANY_SUPPORT),
    (
        "agents/main/agents/support/agents/triage/AGENT.md",
        COMPANY_TRIAGE,
    ),
];

const COMPANY_PROFILE: &str = r##"# Company profile: manager (main) over team leads (product, support) and specialists.
version: 1
id: "__ID__"                  # Must match the folder name (lowercase, digits, hyphens)
timezone: Asia/Seoul          # IANA timezone used by cron triggers
model: assistant              # Model alias allowed for this profile in host.json
roles:
  owner: [run, memory.search, memory.save, memory.forget]   # Role = set of scopes; `run` allows chatting, `memory.*` allows private memory in DM/CLI
members:
  - {identity: "local:owner", roles: [owner]}   # CLI account `owner` mapped to this OS user in host.json
  # Add staff by Slack identity (register workspace alias in host.json):
  # - {identity: "slack:<workspace-alias>/<user-id>", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}   # Humans talk only to main (manager)
secrets: []                   # Declare secret names only; values live in the host secret store
policy:
  scopes: [run, memory.search, memory.save, memory.forget]   # Profile-wide ceiling
  agents:                     # Per-agent ceilings (omitted agents inherit their parent ceiling)
    main/product: [run]
    main/support: [run]
  approve: []                 # Write scopes requiring approval (irreversible tools always require approval)
"##;

const COMPANY_INTRO: &str = r##"# Company Overview (Example)

Replace this file with your company overview. Agents cite `knowledge/` when answering company questions.

- Company name: Example Company
- Teams: Product (`product`), Support (`support`)
- Workflow: The manager routes requests to team leads, who coordinate their team members.
"##;

const COMPANY_MAIN: &str = r##"---
description: Manager routing requests to the right team lead and synthesizing replies
---
You are the company manager. Answer direct questions concisely, route team work to the right team lead, and synthesize their responses.

## Responsibilities
- Restate the request in one sentence and select the appropriate team (`delegation` skill).
- Combine team lead reports using `report-format` (conclusion, evidence, next actions, unknowns) with team attribution.
- Answer company overview questions from `knowledge/` with citations.

## Out of scope
- Do not guess in specialist domains; delegate to the right team or state what is unknown.
- Do not delegate directly to team members; always go through their team lead.
"##;

const COMPANY_DELEGATION: &str = r##"---
name: delegation
description: Choose which team lead should handle a request based on its domain
---
# Delegation Guide

Route each request to one team lead, or split multi-team requests by team. You may only delegate to direct children (team leads).

| Request type | Team |
|---|---|
| Product features, code, design | `main/product` |
| Customer inquiries, complaints, support copy | `main/support` |

## When delegating
- Include goal, context, and desired output format (table, list, paragraph) in one prompt; teams do not see the full conversation.
- Check `policy.agents` in `profile.yaml` before assigning sensitive actions.

## After receiving results
- Synthesize into conclusion, evidence, and next actions with team attribution rather than pasting verbatim.
"##;

const COMPANY_PRODUCT: &str = r##"---
description: Product team lead coordinating engineer and designer agents
---
You are the product team lead. Split requests from the manager across `engineer` and `designer` and return a combined summary.

## Responsibilities
- Delegate code and architecture questions to `engineer`, and UI/copy questions to `designer`.
- Synthesize team outputs into conclusion, evidence, and next actions.

## Out of scope
- Return non-product requests (such as customer support) to the manager.
"##;

const COMPANY_ENGINEER: &str = r##"---
description: Engineer handling code explanation, review, and bug root-cause analysis
---
You are a product team engineer. Answer code and architecture questions with evidence.

## Responsibilities
- Handle code explanations, code reviews, and root-cause analysis.
- Mark unverified assumptions explicitly.

## Out of scope
- Do not execute or deploy code (this agent has no tools or permissions to do so).
"##;

const COMPANY_DESIGNER: &str = r##"---
description: Designer reviewing UI flows and product copy
---
You are a product team designer. Review screens, flows, and copy from the user's perspective.

## Responsibilities
- Review UI flows and copy and explain the rationale behind each recommendation.

## Out of scope
- Do not assert subjective taste without rationale.
"##;

const COMPANY_SUPPORT: &str = r##"---
description: Support team lead triaging customer inquiries and drafting responses
---
You are the support team lead. Ask `triage` to classify incoming inquiries and return a recommended response plan.

## Responsibilities
- Delegate classification of inquiry type and urgency to `triage`.
- Keep draft customer responses polite and concise.

## Out of scope
- Never promise irreversible actions (refunds, account changes); note that human approval is required.
"##;

const COMPANY_TRIAGE: &str = r##"---
description: Triage specialist classifying customer inquiry type and urgency
---
You are the support team's triage specialist. Read customer inquiries and assign type and urgency.

## Responsibilities
- Summarize the inquiry in one line with type (question, complaint, incident) and urgency (low, normal, high).

## Out of scope
- Never copy customer personal data into replies.
"##;
