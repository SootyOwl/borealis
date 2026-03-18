# First-time user feedback: crosslink swarm on a greenfield Rust project

**Summary: powerful, but fragile.**

I used `crosslink swarm` to build a Rust project (Borealis — a multi-channel bot runtime) from a design doc. 2 phases, 13 agents total, ~20k lines of Rust produced. The end result was impressive — once things were working, the swarm wrote a lot of good code in parallel. But getting there involved a lot of manual intervention and debugging.

## Setup & onboarding friction

### Claude doesn't know how to use crosslink
The documentation examples show natural language ("Build this feature across multiple phases"), but Claude had no idea how to use the `crosslink` CLI commands. Following the docs' conversational style didn't work — I had to specifically instruct Claude to use the `crosslink` commands, and even then it guessed wrong frequently. I ended up linking Claude to the raw markdown files on the crosslink repo so it had *some* idea of the workflow. A crosslink skill for Claude Code would solve this entirely.

### Issue ID format inconsistency
Sometimes crosslink issues were keyed with "L" instead of "#" (like "L1" vs "#1"), which broke the hooks. I ended up having to nuke `.crosslink` and `.claude` directories a couple of times to recover from this. Not sure what triggers the inconsistency.

### `/design` open questions workflow
The `/design` skill's open questions feature is great — it caught things in my spec that needed more definition. But the workflow for resolving them is clunky: I had to manually go into the `.design/` file, edit the open question blocks, and then run `/design --continue` to have Claude process my answers. I'd prefer Claude to interview me directly and resolve them conversationally, then update the doc itself.

## Swarm orchestration issues

### 1. Agent status tracking is inconsistent
`swarm status` determines agent state from kickoff heartbeats, but `swarm merge` reads from the phase JSON file where statuses are never updated from "running" to "completed". I had to manually edit `phase-1.json` to set all agents to "completed" before merge would even look at them. These should use the same source of truth.

### 2. `swarm merge` couldn't find any worktrees
Even after fixing the phase JSON statuses to "completed", `swarm merge` reported "No agent worktrees with changes found" — despite 8 worktrees existing with committed code ahead of main. I never got it to work after multiple attempts (with and without `--agents`, `--dry-run`, etc). Fell back to manual git merges, and ultimately had a background agent read all 8 worktrees and write a unified codebase. Unclear what path/naming convention merge expects.

### 3. Stale locks accumulate and are hard to clean up
Agents claim locks on issues but never release them when they finish or crash. `crosslink locks release <id>` requires an agent identity, so the driver/human can't clear them. I had to manually delete lock JSON files from the hub-cache git repo and commit. There should be a `crosslink locks clear-stale` command or a way for the driver to force-release locks.

### 4. `swarm status` provides no progress indication
Status only shows RUNNING/DONE — no indication of whether an agent is actively writing code, stuck on a permission prompt, or sitting idle at a finished prompt. I had to use `tmux capture-pane`, `git log --oneline main..HEAD`, and `ps --ppid` checks to determine actual progress. Even a "last heartbeat: 2m ago" or "commits: 3" would help enormously. This was the single biggest source of confusion — I sat there for several minutes thinking "is it doing anything?" with no way to tell from crosslink alone.

### 5. Agents launched outside swarm can't be adopted
When REQ-13 failed to launch via `swarm launch` (index.lock race), I launched it manually with `crosslink kickoff run`. The swarm had no way to adopt this agent — it stayed as "failed/planned" in the phase JSON even though the manually-launched agent completed successfully on a slightly different branch name. There should be a way to associate an external agent/branch with a swarm slot.

### 6. `swarm launch` index.lock race condition
Launching 8 agents rapidly caused a git index.lock collision on the hub-cache, failing 1 of 8 agents. The lock file then had to be manually removed before the agent could be retried. Presumably the rapid parallel writes to the hub branch need serialization.

### 7. Greenfield parallel agents create unmergeable code
This is the biggest workflow issue: 8 agents each independently created `Cargo.toml`, `src/main.rs`, and `src/lib.rs` from scratch, making every single merge conflict on these shared files. The manual merge was abandoned after the second conflict, and I had a background agent read all 8 worktrees and write a unified codebase instead.

For greenfield projects, the swarm should either:
- Scaffold shared files first (a "phase 0" that creates the project skeleton before parallel agents start)
- Have agents write *only* their module directories and leave integration to a dedicated step
- Or at minimum, warn the user that greenfield + parallel agents = painful merges

### 8. `.kickoff-status` file not written reliably
The mechanism for marking agents as DONE is a `.kickoff-status` file in the worktree. Some agents that clearly finished their work (committed code, posted result comments) never wrote this file — seemingly because their crosslink session dropped mid-flow, which caused the work-check hook to block the cleanup steps. The agent ends up in a state where it finished the actual work but can't complete its own bookkeeping.

### 9. Session tracking fragility cascades into hook failures
`crosslink quick` creates an issue and sets it as active work, but if the issue is subsequently closed, `crosslink session work <id>` on the closed issue doesn't register. This causes the work-check hook to block all subsequent tool calls (Write, Edit, Bash) until a new issue is created. For agents running autonomously, this is a death spiral — they can't create a new issue because the Bash command to do so is also blocked by the hook (unless the command starts with `crosslink `). Several agents hit variations of this.

### 10. Agent stalling without indication
One agent (REQ-8, the scheduler) stalled mid-work — it stopped generating output and sat at an idle prompt with 5 of 7 tasks remaining. Crosslink still showed it as "RUNNING" with no indication anything was wrong. I had to manually send "continue" to its tmux session to restart it.

There *is* a timeout system (`timeout_secs` in `.kickoff-metadata.json`, default 3600s), but it's a hard wall-clock limit, not an activity/heartbeat timeout. An agent that stalls after 9 minutes of work looks identical to one actively working for the remaining 51 minutes. What's needed is an *inactivity* timeout — if no tool calls or file writes happen for N minutes, flag the agent as potentially stalled rather than waiting for the full hour to expire.

## Knowledge management

### No `crosslink knowledge update` or `--from-doc` on `knowledge edit`
I had to `crosslink knowledge remove` + `crosslink knowledge add` to update a knowledge page because `knowledge edit` doesn't accept `--from-doc` and there's no `knowledge update` command. Minor but annoying when iterating on a design doc.

## What worked well

Despite the issues above, the core concept is sound and the results were impressive:

- **`/design` skill** was excellent for going from a rough spec to a validated, structured design document with requirements, acceptance criteria, and architecture references
- **Gap analysis** (`crosslink kickoff plan`) caught real issues — missing crate dependencies, unspecified schemas, underspecified parameters
- **Parallel agent execution** genuinely worked — 8 agents wrote ~20k lines of Rust in parallel, each producing real, tested, working code in their own modules
- **Phase gating** with `swarm gate` running the test suite before advancing was a good safety net
- **Issue tracking integration** meant every agent's work was traceable back to a requirement

The overall impression is that crosslink is doing something genuinely useful for multi-agent development, but the operational reliability needs hardening — especially around status tracking, lock management, merge tooling, and the interaction between hooks and agent lifecycle. Most of my time was spent debugging crosslink state rather than reviewing the code the agents wrote.
