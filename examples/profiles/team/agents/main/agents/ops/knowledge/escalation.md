# Incident Severity and Escalation Order (Example)

Synthetic example. Replace with your team's actual policy.

| Level | Criteria | Initial Response | Escalation Order |
|---|---|---|---|
| SEV1 | Full outage or data loss | Mitigate immediately; post updates every 15 minutes | Ops -> Lead -> Entire team |
| SEV2 | Partial degradation of core features | Mitigate within 1 hour | Ops -> Lead |
| SEV3 | Inconvenience with a workaround | Resolve within business hours | Ops |

- When severity is unclear, classify one level higher and downgrade once stabilized.
- Irreversible actions (rollbacks, data deletion, account suspension) require human approval regardless of severity.
