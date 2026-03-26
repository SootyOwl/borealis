---
allowed-tools: Bash(jj *), Bash(git *)
description: Commit current changes with a well-formed message
---

## Context

- Working tree status: !`jj st 2>/dev/null || git status --short`
- Current change: !`jj log --limit 3 2>/dev/null || git log --oneline -3`

## Your task

The user wants to commit their current changes.

### 1. Review changes

Run `jj diff --stat` to see what changed. Review the full diff with `jj diff` if needed.

### 2. Write the commit message

- Summarize what changed and why (1-2 sentences)
- Follow conventional commit style (e.g. `feat(tools): add web search via Jina AI`)
- Use imperative mood

### 3. Create the commit

Since jj's working copy IS a commit, describe it and create a new empty change on top:

```bash
jj desc -m "<message>"
jj new
```

### 4. Show summary

Display:
- The change ID and description
- Files changed summary

## Constraints

- Never force-push without explicit user request.
- Never use `jj restore` without confirming with the user.
- If the working copy has no changes, don't create an empty commit.
