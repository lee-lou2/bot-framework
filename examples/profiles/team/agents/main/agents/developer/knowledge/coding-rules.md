# Coding Rules (Example)

Synthetic example. Replace with your team's actual rules.

- Each function does one thing and starts with a verb.
- Never swallow errors; add context and propagate anything you cannot handle.
- Pair new behavior with tests. For bug fixes, write a failing test first.
- Never put secrets in code, logs, or test fixtures.
- Validate inputs once at the boundary; inner functions trust validated values.
- Comment the "why"; let names and structure explain the "what".
- Keep each change focused on one purpose and small enough to review within a day.
