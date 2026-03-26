#!/usr/bin/env python3
"""
PreToolUse hook: prevent git commands that corrupt jj-colocated repos.

In a colocated repo (.jj/ + .git/), git mutating commands bypass jj's
tracking and can corrupt state. This hook blocks them and suggests the
jj equivalent.
"""

import json
import sys
import os

# Only guard Bash tool calls
input_data = json.loads(sys.stdin.read())
tool_name = input_data.get("tool_name", "")

if tool_name != "Bash":
    json.dump({"decision": "allow"}, sys.stdout)
    sys.exit(0)

# Check if we're in a jj repo
repo_root = os.environ.get("CLAUDE_PROJECT_DIR", ".")
if not os.path.isdir(os.path.join(repo_root, ".jj")):
    json.dump({"decision": "allow"}, sys.stdout)
    sys.exit(0)

command = input_data.get("tool_input", {}).get("command", "")

# Dangerous git commands and their jj equivalents
BLOCKED = {
    "git commit": "Use: jj desc -m 'message' && jj new",
    "git add": "Not needed — jj auto-tracks all changes",
    "git stash": "Use: jj new (changes stay in current change)",
    "git checkout": "Use: jj edit <change-id> or jj new <parent>",
    "git switch": "Use: jj edit <change-id> or jj new <parent>",
    "git merge": "Use: jj new <change1> <change2> (creates merge commit)",
    "git rebase": "Use: jj rebase -s <source> -d <dest>",
    "git reset": "Use: jj restore or jj abandon",
    "git cherry-pick": "Use: jj duplicate <change-id>",
}

# Allow safe read-only git commands
SAFE_GIT = [
    "git status", "git diff", "git log", "git show", "git branch",
    "git remote", "git fetch", "git worktree list", "git rev-parse",
    "git config", "git ls-files", "git describe", "git reflog",
    "git cat-file", "git for-each-ref",
]

# Check if it's a git command
cmd_stripped = command.strip()
if not cmd_stripped.startswith("git "):
    json.dump({"decision": "allow"}, sys.stdout)
    sys.exit(0)

# Allow safe commands
for safe in SAFE_GIT:
    if cmd_stripped.startswith(safe):
        json.dump({"decision": "allow"}, sys.stdout)
        sys.exit(0)

# Block dangerous commands
for blocked, suggestion in BLOCKED.items():
    if cmd_stripped.startswith(blocked):
        json.dump({
            "decision": "block",
            "reason": f"This is a jj repo. '{blocked}' will corrupt state. {suggestion}"
        }, sys.stdout)
        sys.exit(0)

# Allow git push (needed for remote sync)
if cmd_stripped.startswith("git push"):
    json.dump({"decision": "allow"}, sys.stdout)
    sys.exit(0)

# Block any other git write commands we didn't explicitly allow
json.dump({
    "decision": "block",
    "reason": f"Unknown git command in jj repo. Use jj commands instead, or add this to the safe list if it's read-only."
}, sys.stdout)
