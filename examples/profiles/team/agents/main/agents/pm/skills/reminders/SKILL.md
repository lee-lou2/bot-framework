# Reminders

Schedule recurring or deferred tasks in chat with `schedule.create`. Provide an instruction plus either `at` (UTC RFC 3339) or `cron` (5 fields, evaluated in the profile timezone `Asia/Seoul`).

- One-off: `{"at": "2030-01-01T01:00:00Z", "instruction": "Check the Thursday release checklist"}`
- Recurring: `{"cron": "0 17 * * 5", "instruction": "Remind the team to prepare weekly retro items"}`
- List schedules with `schedule.list`; cancel with `schedule.cancel` (`{"id": "<schedule-id>"}`).

Scheduled messages return to the creator's original conversation. On CLI, you cannot target another recipient. Write self-contained instructions stating what to do, why, and for whom.

Pre-configured profile cron runs belong in `agents/main/triggers.yaml` (e.g., `standup`), not this skill. Chat schedules created by users and operator-declared triggers operate independently.
