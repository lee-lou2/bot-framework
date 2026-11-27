# Team Profile Template

English | [한국어](ko/team-template.md)

[`examples/profiles/team`](../examples/profiles/team) is a copyable multi-agent profile with one team lead (`main`) delegating to four specialists (`developer`, `designer`, `pm`, `ops`). It demonstrates every profile primitive—rules, knowledge, skills, memory seeds, non-secret env, cron triggers, and scoped approvals—and runs out of the box on the built-in demo host. All people, channels, and company details are synthetic.

This guide lives outside the profile directory because profile roots accept only known entries (`profile.yaml`, `rules.md`, `agents`, `knowledge`, `skills`).

## Try It First

From the repository root (with the same PostgreSQL setup as the README demo; the built-in demo host knows both `personal` and `team`):

```sh
export BOT_PROFILES="$PWD/examples/profiles"
bot validate                                                     # Validates personal and team
bot map --profile team                                           # Shows delegation tree, caps, skills, and tools
bot chat --profile team "Show today's agenda"                    # Demo model calls agenda.list
bot chat --profile team --request-id team-1 "Delete the walk event"  # Opens an approval card in Admin UI
```

Approve the pending card in `Admin UI: http://...`. The demo model does not invoke `delegate`; connect a real model alias in `host.json` to exercise sub-agent delegation. Directory structure, permission ceilings, and approval gates are validated identically in demo mode.

## Directory Layout

```text
examples/profiles/team/
  profile.yaml                      Roles, members, bindings, permission caps
  rules.md                          Shared rules prepended to every agent
  knowledge/                        Team-wide reference docs (.md, .txt)
    team-intro.md  glossary.txt
  skills/                           Shared skills visible to all agents
    report-format/SKILL.md
  agents/main/                      Team lead (all entrances bind here)
    AGENT.md  env.yaml  triggers.yaml
    memory/team-profile.md          Bootstrap shared memory
    knowledge/working-agreements.md
    skills/
      delegation/SKILL.md           Routing guide for sub-agents
      agenda/{SKILL.md,tools.yaml}  Example tool binding (demo-agenda)
    agents/
      developer/                    Code explanation, review, bug analysis (read-only)
        AGENT.md  knowledge/coding-rules.md  skills/code-review/SKILL.md
      designer/                     UI/UX review, design principles, accessibility
        AGENT.md  knowledge/design-principles.md  skills/design-review/SKILL.md
      pm/                           Specs, schedules, priorities, reminders
        AGENT.md  memory/priorities.md  skills/{spec-writing,reminders}/SKILL.md
      ops/                          Health checks, incident runbooks (gated writes)
        AGENT.md  env.yaml  knowledge/escalation.md  skills/incident-runbook/SKILL.md
```

**Folder hierarchy defines delegation and inheritance.** `main/ops` receives `rules.md` -> `main/AGENT.md` -> `ops/AGENT.md`. Each agent sees only its own and ancestors' skills and knowledge (never siblings'). `env.yaml` inherits top-down with child overrides (`ops/env.yaml` overrides `TEAM_TIMEZONE` to `UTC`). Memory seeds are not inherited. Delegation is direct-child only.

## File Responsibilities

| File | Responsibility |
|---|---|
| `profile.yaml` | Profile `id` (matches folder), `timezone`, `model`, `roles`, `members`, `bindings`, `secrets` (names only), and `policy` (`scopes`, `agents`, `approve`). |
| `rules.md` | Profile-wide rules prepended to every agent prompt. |
| `knowledge/` | Retrieved on demand with citations (up to 8 docs / 32 KiB per request); agent `knowledge/` is visible to that agent and its descendants. |
| `skills/<name>/SKILL.md` | Procedural instructions; profile `skills/` is shared by all agents, agent `skills/` by that agent and descendants. |
| `skills/<name>/tools.yaml` | Tool declarations: `server` alias, `id`, risk `class` (`read`/`write`/`irreversible`), `scopes`, `input`, `env`, `secrets`, `egress`. |
| `agents/*/AGENT.md` | Role prompt with optional YAML front matter (`description`, `builtin`, `model`). |
| `agents/*/env.yaml` | Non-secret key/value map; keys reach a tool only when listed in that tool's `env`. |
| `agents/*/triggers.yaml` | Static cron, event, or HTTP triggers (this template defines a weekday 09:00 standup summary). |
| `agents/*/memory/*.md` | One-time bootstrap memory seeds; requires member `owner`, `visibility` defaults to `private`. |

## Permissions at a Glance

Effective scopes are the three-way intersection: `role scopes ∩ policy.scopes ∩ policy.agents` (omitted agents inherit their parent's cap).

| Agent | Scope ceiling | Capabilities |
|---|---|---|
| `main` | Per caller role (`lead` gets all) | Agenda lookup, delegation, scheduling |
| `main/developer` | `run` | No MCP tools; uses built-in `read` to inspect code |
| `main/designer` | `run` | No tools; prose and table reviews only |
| `main/pm` | `run`, `calendar.read`, `schedule.*` | Read-only agenda plus chat scheduling (`create`/`list`/`cancel`) |
| `main/ops` | `run`, `calendar.read`, `calendar.delete` | May request `agenda.delete`, which is `irreversible` and requires an approval card on every call |

Risky actions are constrained at three layers:
1. **Tool contract:** `class: irreversible` enforces human approval before execution even without `policy.approve` (`agenda.delete`).
2. **Rules & prompts:** `rules.md` and `ops/AGENT.md` require stating target, blast radius, and rollback plan before acting.
3. **Runbooks & knowledge:** `incident-runbook` and `escalation.md` define approval criteria and contact order.

The weekday digest trigger runs as `service:standup` (`digest` role: `run`, `calendar.read`). Service principals are non-human and cannot approve actions, so approval-gated tools cannot be automated this way.

## Adapt for Your Team

1. **Copy:** `cp -R examples/profiles/team profiles/<your-team>` (place only profile directories directly under `BOT_PROFILES`), or scaffold a fresh workspace with `bot init <your-team> --template team`.
2. **Set ID:** Match `profile.yaml` `id` to the folder name (lowercase, digits, hyphens). Keep `policy.agents` paths as-is.
3. **Replace synthetic content:** Edit or remove the sample files under `knowledge/` and `memory/` (every memory seed `owner` must exist in `members`).
4. **Configure members and entrances:** Replace `local:team` with real identities and bind host-registered entrances (CLI accounts or Slack workspaces/channels; uncomment the Slack lines in `profile.yaml`). Each entrance must be unique across active profiles.
5. **Swap tools:** Replace `agents/main/skills/agenda` with your real MCP servers (or remove the folder, drop `calendar.*` scopes from `roles`/`policy`, and update `AGENT.md`/`triggers.yaml`).
6. **Keep secrets out of profiles:** Declare only secret **names** in `profile.yaml` `secrets` and `tools.yaml` `secrets`; store values in the host secret store. `bot validate` rejects missing secrets.
7. **Adjust schedule & timezone:** Update `timezone`, `triggers.yaml`, and `env.yaml` (`TEAM_TIMEZONE`), or set `triggers: []`.
8. **Register on the host:** Add your profile ID to `models` and register entrances in `host.json` ([profile-spec.md](profile-spec.md#host-configuration-for-a-new-profile)). The demo host only knows `personal` and `team`.
9. **Verify:** Run `bot validate`, `bot doctor`, and `bot map --profile <your-team>`.

## Add Another Sub-Agent

To add a `support` agent under `main`:

```sh
bot add agent main/support --dir profiles/<your-team>
bot add skill triage --agent main/support --dir profiles/<your-team>
```

1. Fill in `description` (<= 200 chars, single line) and instructions in `agents/main/agents/support/AGENT.md`, plus optional `knowledge/`, `skills/`, `memory/`, or `env.yaml` (skill names cannot collide with visible ancestors).
2. Add an explicit cap in `profile.yaml` under `policy.agents` (e.g. `main/support: [run]`); omitting it inherits `main`'s full cap.
3. Add a routing row to `agents/main/skills/delegation/SKILL.md` so `main` knows when to delegate.
4. Nest deeper under `agents/<name>/` if needed (up to 8 levels; delegation remains direct-child only).
5. Verify with `bot validate` and `bot map --profile <your-team>`.

All names must match `^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$` (<= 63 chars). Unknown filenames (such as a misspelled `trigger.yaml`) fail validation rather than being silently ignored.

## Good to Know

- `main/developer` declares `builtin: [read]`, which requires a `claude-cli` VM with verified read isolation. Remove `builtin` if running `main/developer` on a direct API provider (`anthropic-api`, `openai-api`; see [architecture.md](architecture.md)).
- `main` and `main/pm` share the same chat scheduling tools across `anthropic-api`, `openai-api`, and `claude-cli`: `lead` can create, list, and cancel schedules; `member` can list. Targeting another registered Slack channel requires owner approval at creation and approval of each `Delivery draft` card on firing. Use `triggers.yaml` for profile-level recurring jobs; service principals cannot create user schedules ([plugins.md](plugins.md#schedules-and-follow-up-wakes)).
- All sample names (`alice`, `bob`, etc.) are synthetic; real identities and secrets live solely in `host.json`, `members`, and the secret store.

Related: [Profile Specification](profile-spec.md) · [Security](security.md) · [MCP Tools](plugins.md) · [Gateway](gateway.md)
