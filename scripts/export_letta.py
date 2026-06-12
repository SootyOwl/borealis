#!/usr/bin/env python3
"""Export an agent from a self-hosted Letta server into the JSON layout
expected by `cargo run -- migrate-letta --source <dir>`.

Produces three files in the output directory:
  - core_memory.json     ({"blocks": [...]})
  - archival_memory.json ([{...}, ...])
  - messages.json        ([{...}, ...] in chronological order)

Usage:
  export LETTA_BASE_URL=http://localhost:8283
  export LETTA_API_KEY=...            # optional, if SECURE=true
  python3 scripts/export_letta.py --agent-id <id> --out ./letta-export

Or, if you don't know the agent id:
  python3 scripts/export_letta.py --list
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Any

import urllib.error
import urllib.parse
import urllib.request


def http_get(url: str, headers: dict[str, str]) -> Any:
    req = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        print(f"HTTP {e.code} on {url}\n{body}", file=sys.stderr)
        raise


def paginate(
    base_url: str,
    path: str,
    headers: dict[str, str],
    page_size: int = 100,
    cursor_param: str = "after",
    id_field: str = "id",
) -> list[dict]:
    """Generic cursor pagination: fetch `limit` items, use last item's id as `after`."""
    items: list[dict] = []
    cursor: str | None = None
    while True:
        params = {"limit": str(page_size)}
        if cursor:
            params[cursor_param] = cursor
        url = f"{base_url}{path}?{urllib.parse.urlencode(params)}"
        batch = http_get(url, headers)
        if not isinstance(batch, list):
            # Some endpoints wrap results in an object; try common shapes.
            batch = batch.get("data") or batch.get("items") or []
        if not batch:
            break
        items.extend(batch)
        if len(batch) < page_size:
            break
        last_id = batch[-1].get(id_field)
        if not last_id or last_id == cursor:
            break
        cursor = last_id
        print(f"  ... {len(items)} fetched", file=sys.stderr)
    return items


def list_agents(base_url: str, headers: dict[str, str]) -> None:
    agents = http_get(f"{base_url}/v1/agents/?limit=100", headers)
    if not isinstance(agents, list):
        agents = agents.get("data", [])
    for a in agents:
        print(f"{a.get('id')}\t{a.get('name')}\t{a.get('created_at', '')}")


def export_core_memory(
    base_url: str, agent_id: str, headers: dict[str, str]
) -> dict:
    print("Exporting core memory blocks...", file=sys.stderr)
    blocks = http_get(
        f"{base_url}/v1/agents/{agent_id}/core-memory/blocks", headers
    )
    if not isinstance(blocks, list):
        blocks = blocks.get("blocks") or blocks.get("data") or []
    print(f"  got {len(blocks)} blocks", file=sys.stderr)
    # migrate.rs expects {"blocks": [...]} with value/label/description fields.
    return {"blocks": blocks}


def probe_path(base_url: str, path: str, headers: dict[str, str]) -> bool:
    """Return True if a GET to base_url+path returns 2xx."""
    url = f"{base_url}{path}?limit=1"
    req = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return 200 <= resp.status < 300
    except urllib.error.HTTPError as e:
        return 200 <= e.code < 300
    except Exception:
        return False


def export_archival(
    base_url: str, agent_id: str, headers: dict[str, str]
) -> list[dict]:
    print("Exporting archival memory passages...", file=sys.stderr)
    # Letta has used several paths across versions; probe until one works.
    candidates = [
        f"/v1/agents/{agent_id}/archival_memory",
        f"/v1/agents/{agent_id}/archival-memory",
        f"/v1/agents/{agent_id}/archival",
        f"/v1/agents/{agent_id}/memory/archival",
        f"/v1/agents/{agent_id}/passages",
        f"/v1/agents/{agent_id}/memory/passages",
    ]
    chosen = None
    for path in candidates:
        if probe_path(base_url, path, headers):
            chosen = path
            break
    if chosen is None:
        print(
            "  WARNING: could not find archival endpoint on this Letta version.\n"
            "  Tried: " + ", ".join(candidates) + "\n"
            "  Writing empty archival_memory.json — re-run with --skip-archival to silence this.",
            file=sys.stderr,
        )
        return []
    print(f"  using {chosen}", file=sys.stderr)
    passages = paginate(base_url, chosen, headers)
    print(f"  got {len(passages)} passages", file=sys.stderr)
    return passages


def export_messages(
    base_url: str, agent_id: str, headers: dict[str, str]
) -> list[dict]:
    print("Exporting messages...", file=sys.stderr)
    # Letta returns messages oldest-first when using `after` cursor pagination.
    messages = paginate(
        base_url,
        f"/v1/agents/{agent_id}/messages",
        headers,
    )
    print(f"  got {len(messages)} messages", file=sys.stderr)
    return messages


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--base-url",
        default=os.environ.get("LETTA_BASE_URL", "http://localhost:8283"),
        help="Letta server base URL (default: $LETTA_BASE_URL or http://localhost:8283)",
    )
    ap.add_argument(
        "--api-key",
        default=os.environ.get("LETTA_API_KEY"),
        help="Bearer token (default: $LETTA_API_KEY). Only needed if SECURE=true.",
    )
    ap.add_argument("--agent-id", help="Agent ID to export")
    ap.add_argument(
        "--out",
        default="./letta-export",
        help="Output directory (default: ./letta-export)",
    )
    ap.add_argument(
        "--list", action="store_true", help="List all agents and exit"
    )
    ap.add_argument(
        "--skip-messages",
        action="store_true",
        help="Don't export message history",
    )
    ap.add_argument(
        "--skip-archival",
        action="store_true",
        help="Don't export archival memory",
    )
    args = ap.parse_args()

    base_url = args.base_url.rstrip("/")
    headers = {
        "Accept": "application/json",
        # Cloudflare in front of self-hosted Letta blocks python-urllib's default UA.
        "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) borealis-letta-export/1.0",
    }
    if args.api_key:
        headers["Authorization"] = f"Bearer {args.api_key}"

    if args.list:
        list_agents(base_url, headers)
        return 0

    if not args.agent_id:
        print("error: --agent-id is required (or use --list)", file=sys.stderr)
        return 2

    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)

    # --- core memory ---
    core = export_core_memory(base_url, args.agent_id, headers)
    (out_dir / "core_memory.json").write_text(json.dumps(core, indent=2))

    # --- archival ---
    if not args.skip_archival:
        passages = export_archival(base_url, args.agent_id, headers)
        (out_dir / "archival_memory.json").write_text(
            json.dumps(passages, indent=2)
        )

    # --- messages ---
    if not args.skip_messages:
        messages = export_messages(base_url, args.agent_id, headers)
        (out_dir / "messages.json").write_text(json.dumps(messages, indent=2))

    print(f"\nExport complete → {out_dir}", file=sys.stderr)
    print("Next step:", file=sys.stderr)
    print(
        f"  cargo run -- migrate-letta --source {out_dir}", file=sys.stderr
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
