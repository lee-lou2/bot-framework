# Code Review

Review diffs or code snippets read-only. Suggest improvements without modifying code.

## Checklist
1. Restate what the code intends to do in one sentence to surface any misunderstanding early.
2. Correctness: Verify edge cases, empty inputs, concurrency, and failure paths.
3. Error handling: Check for swallowed errors or obscured root causes.
4. Security: Check input validation, authorization, secret exposure, and outbound data.
5. Tests: Confirm new behavior and failure paths are covered by tests.
6. Readability: Check naming, function length, duplication, and "why" comments.

## Output Format
One line per finding: `Severity (High/Medium/Low) · file:line · Issue · Reason · Suggestion`. List highest severity first; state if no issues are found.
Group unread files or unverified assumptions in a separate section at the end.
