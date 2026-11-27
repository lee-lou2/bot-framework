# Isolate the model, gate risky actions through one broker, and commit requests to PostgreSQL first.

English | [한국어](ko/why.md)

Short answers to the recurring "why" questions behind this design. Exact contracts live in the linked specifications.

**In one sentence:** Assume the AI model can be fooled or wrong, so withhold keys and host execution; enforce authorization, human approval, and auditing in one broker; and persist every request to PostgreSQL before acting.

## Why isolate only the model in a VM?

The model is the least trustworthy component because it reads user messages and untrusted external content (prompt injection). When running a local model process (`claude-cli`), only that process runs inside a disposable VM with zero secrets (the host broker holds the provider key; the VM gets only a session token) and a single egress route through the broker relay. Even if tricked, it has no keys to steal and nowhere to exfiltrate data.

Agents that do not need workspace files use direct APIs (`anthropic-api`, `openai-api`): there is no model process to contain, the worker attaches the provider key only when calling upstream, the model has no host filesystem or shell, and every tool call still goes through the broker. ([Model Image](model-image.md) · [Security](security.md))

## Why not put the entire bot in one container?

Gateway, broker, and store handle Slack tokens, database credentials, and tool secrets. Co-locating the model in that container places it next to those secrets. The security boundary separates the trusted host from the untrusted model — independent of how you package or deploy the bot binary itself.

## Why a fresh VM per request instead of a pre-warmed pool?

Inputs, work directory, and session tokens are run-specific and bound at VM creation. Discarding each VM prevents any state from leaking into a subsequent run (including across profiles). On a development Mac, cold VM start-to-exit took under 1 second (excluding mount, network, and firewall setup) — well below typical model latency. If startup overhead ever becomes a bottleneck, a single-use pre-warmed pool can be added later (not yet built).

## Why commit requests to PostgreSQL before executing?

Defining acceptance as a PostgreSQL commit guarantees no request is lost across crashes or restarts:

- Duplicate deliveries resolve to the existing run instead of spawning a second one.
- During human approvals or child delegations, the worker releases its VM and lease and later resumes from the saved session.
- Intake, queue state, approvals, and audit records commit atomically in the same transaction.

The trade-off is a two-step accept-then-execute flow rather than immediate inline execution. ([Reliability](reliability.md))

## Why execute tools outside the model behind a broker?

Tools perform credentialed work; models only ask the broker to invoke them. Each tool receives only its declared secrets and can reach only its declared egress hosts, so a subverted model never sees keys and cannot exceed tool bounds. The broker computes `caller scope ∩ agent cap ∩ tool scope`, enforces approvals, and records audits in one place. Allowing the model to execute tools directly would bypass that gate. ([MCP Tools](plugins.md))

## Why require human approval?

Irreversible side effects (external messages, deletions, writes) require out-of-band human confirmation. Each approval binds cryptographically to exact arguments, so any argument change invalidates it. Models can never approve actions.

## Why disable shell, file-write, and web tools on the model?

They are off by default to minimize attack surface. Even in the `claude-cli` VM, only isolation-backed `read` can be opted into (direct API runners expose no built-in file tools). Wrap any needed capability as an explicitly reviewed tool instead. ([Profile Spec](profile-spec.md))

## When this architecture is not a fit

- **Local coding or PC file editing:** If the model must write/run arbitrary code or modify your local files directly, wrap specific actions as tools or run outside the framework and use this only for delivery and approvals.
- **Single-user, fully trusted input:** An OS sandbox plus approval prompts may be enough. This framework targets multi-user Slack/HTTP bots operating with real credentials; per the [Anthropic Secure Deployment Guide](https://code.claude.com/docs/en/agent-sdk/secure-deployment), it defaults to strong isolation for model execution.
- **Non-Apple VM hosts:** The only documented model VM backend is Apple `container`.
