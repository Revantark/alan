---
name: test-driven-implementation
description: Implement features with comprehensive tests (unit + integration), verify end-to-end, keep code concise with WHY comments, cover blast area, and clearly report manual test steps and coverage gaps. Use when the user asks you to implement, fix, or extend code and expects rigorous test coverage and verification.
---

# Test-Driven Implementation Skill

## When to use
- User asks you to implement, fix, refactor, or extend code.
- User expects tests, verification, and clear reporting of what is/isn't covered.
- User cares about blast-radius safety and manual-test handoff.

## Core conventions (follow all of them)

### 1. Plan before writing code
- Restate the request in 1–3 lines.
- List assumptions and open questions explicitly.
- Identify the **blast area** before touching code (see §6).
- Only ask the user when ambiguity is truly blocking; otherwise proceed with a documented assumption.

### 2. Concise implementation
- Write the smallest correct change that satisfies the requirement.
- No speculative abstractions, no dead code, no "just in case" helpers.
- Prefer editing existing functions over adding parallel ones.
- Do **not** reformat unrelated code, rename unrelated symbols, or "clean up" outside the blast area.

### 3. Comments = WHY, not WHAT
- Every non-trivial comment answers *why* (constraint, tradeoff, invariant, gotcha).
- Never narrate *what* the code already says (e.g., `// increment i`).
- Non-obvious decisions must have a WHY comment (e.g., ordering, retry choice, workaround for library bug with link/issue reference).
- If you cannot justify why a comment is needed, delete it.

### 4. Tests for everything
- **Unit tests** for every new/changed pure function, branch, edge case, and error path.
- **Integration tests** whenever the change crosses a boundary (DB, HTTP, filesystem, queue, third-party API, IPC, CLI↔lib).
- Cover: happy path, boundary values, invalid input, error/exception paths, concurrency if relevant, and regression for the original bug (if fixing).
- Prefer real dependencies over mocks in integration tests; mock only at true external boundaries.
- Tests must be deterministic, isolated, and independent of execution order.

### 5. Verify end-to-end
- Run the full relevant test suite locally (not just the new files).
- Run linters/formatters/type-checkers used by the project.
- If a build step exists, build.
- If the project has a smoke/integration/e2e script, run it.
- Report exact commands run and their outcomes.

### 6. Blast area coverage
The **blast area** = every file, function, module, endpoint, or schema touched *directly or transitively* by the change. Before finishing:
- Enumerate the blast area explicitly.
- Ensure each blast-area surface has either (a) an existing test that exercises the changed behavior, or (b) a new test you added.
- If a blast-area surface cannot be tested automatically (e.g., UI, infra, hardware, third-party prod account), list it under **Manual tests required**.

### 7. Manual test handoff
Only the user can perform some checks. For those, provide a **Manual Test Plan** containing:
- What to test (surface + scenario)
- Preconditions / setup
- Exact steps
- Expected result
- Why it can't be automated (briefly)

Do not claim something "works" if its only verification is manual and not yet performed.

### 8. Final report format (always include)
Produce a report with these four sections, in this order:

