---
allowed-tools: Bash(tmux *), Bash(jj *), Bash(git *), Bash(cat *), Bash(ls *)
description: Check on background subagents running in tmux sessions
---

## Context

- Active tmux sessions: !`tmux list-sessions 2>/dev/null`
- Current worktrees: !`git worktree list 2>/dev/null`

## Your task

Check all active subagent tmux sessions and report their status.

### 1. Find agent sessions

List tmux sessions. Agent sessions typically have descriptive names.

### 2. For each session

a. **Capture terminal state:**
```bash
tmux capture-pane -t <session-name> -p -S -20
```

b. **Check if Claude is running:**
```bash
tmux display-message -t <session-name> -p '#{pane_current_command}'
```

c. **Determine status:**
- **Working**: Claude is running, tool calls visible
- **Idle**: Claude is running but no recent output
- **Exited**: Pane command is `bash` (Claude exited)
- **Waiting**: Permission prompt or question visible

### 3. Report

```
Subagents:
  <session>  [Working|Idle|Exited|Waiting]  <what it's doing>
```

### 4. Actions

- **If working**: "Check back later, or attach: `tmux attach -t <name>`"
- **If exited**: "Claude exited. Resume with: `tmux send-keys -t <name> 'claude --continue' Enter`"
- **If waiting**: Read the prompt and ask the user what to answer.
