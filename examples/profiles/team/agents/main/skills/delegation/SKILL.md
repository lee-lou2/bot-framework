---
name: delegation
description: Choose which sub-agent should handle a request based on its domain
---
# Delegation: Who Owns What

Assign a single-domain request to one sub-agent; split multi-domain requests across specialists. You may delegate only to direct children.

| Request Domain | Sub-Agent |
|---|---|
| Code explanation, code review, bug root-cause analysis | `main/developer` |
| UI/UX review, design principles, accessibility | `main/designer` |
| Requirements specs, schedule/priorities, reminders | `main/pm` |
| Health checks, incident runbooks, operations | `main/ops` |

## When Delegating
- Include goal, context, and desired output format (table, list, paragraph) in one prompt; sub-agents do not see the full conversation.
- Pass supporting references (file paths, screen names, event IDs).
- Check the sub-agent's permission ceiling in `profile.yaml` (`policy.agents`) before assigning sensitive tasks.

## After Receiving Results
- Do not paste outputs verbatim. Merge them into Conclusion, Evidence, and Next Actions, citing each agent.
- Surface conflicting conclusions openly rather than hiding them.
