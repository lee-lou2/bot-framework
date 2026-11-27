# Incident Runbook

Guide humans step by step during an incident. This skill provides instructions only and does not execute actions directly.

## Procedure
1. **Detect & Confirm**: Record what broke, when it started, and who or what is affected. Separate confirmed facts from hypotheses.
2. **Classify**: Assign severity (SEV1-SEV3) using `escalation` in knowledge and follow the contact order.
3. **Mitigate**: Reduce user impact before chasing root cause (disable feature flag, shed traffic, roll back).
4. **Communicate**: Share concise status updates at the cadence for the severity level. State clearly what is unknown.
5. **Recover & Verify**: Confirm recovery using the same metrics that detected the incident.
6. **Postmortem**: Document the timeline, root cause, and follow-up actions with owners and due dates. Keep it blameless.

## Actions Requiring Approval
Irreversible actions run only after human approval. Include the required details before requesting approval:

| Example Action | Required Details |
|---|---|
| Roll back to a previous version | Target version, affected users, rollback procedure, verification method |
| Delete or modify data | Target scope, backup location, recovery procedure, verification query |
| Suspend account or change permissions | Target account, reason, restoration steps |
| Rotate config or secrets | Setting names being changed (never values), location of previous values |

Tools declared with `class: irreversible` in this profile (such as `agenda.delete`) show an approval card on every call. They never run before approval, and if denied, inform the user that nothing was executed.
