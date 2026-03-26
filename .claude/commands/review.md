---
allowed-tools: Bash(jj *), Bash(git *), Bash(cargo *), Read, Grep
description: Pre-commit quality gate — review changes before committing
---

## Context

- Changes: !`jj diff --stat 2>/dev/null || git diff --stat`

## Your task

Run through this review before committing.

### 1. Review full diff

```bash
jj diff
```

Look for: correctness, edge cases, security (injection, hardcoded secrets, unsafe patterns).

### 2. Scan for stub patterns

Search for: `TODO`, `FIXME`, `HACK`, `XXX`, `unimplemented!()`, `todo!()`, empty function bodies.
If found: fix them now.

### 3. Check for debug leftovers

Search for: `dbg!()`, debugging `println!`, commented-out code blocks (3+ consecutive).
If found: remove them.

### 4. Run lint and format

```bash
cargo clippy -- -D warnings
cargo fmt --check
```

Fix any issues.

### 5. Run tests

```bash
cargo test
```

All tests must pass.

### 6. Print checklist

```
Review checklist:
  [PASS/FAIL] No stub patterns
  [PASS/FAIL] No debug leftovers
  [PASS/FAIL] Lint clean
  [PASS/FAIL] Format clean
  [PASS/FAIL] Tests pass
```

Fix failures, then proceed to `/commit`.
