#!/usr/bin/env bash
# Read-only look at a running (or finished) sortie session for one issue.
#   sortie/peek.sh <issue-number> [events]   # default: last 12 events
#   sortie/peek.sh <issue-number> -f         # follow new events
# Reads the Claude Code transcript JSONL for the issue's workspace and the
# workspace's git state. Never writes anything.
set -euo pipefail
n="${1:?issue number}"; shift || true
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WS="$REPO_ROOT/build/sortie-workspaces/$n"
[ -d "$WS" ] || { echo "no workspace for issue $n at $WS" >&2; exit 1; }
slug="$(printf '%s' "$WS" | tr '/.' '--')"
dir="$HOME/.claude/projects/$slug"
f="$(ls -t "$dir"/*.jsonl 2>/dev/null | head -1 || true)"

echo "== workspace $WS"
git -C "$WS" log --oneline -3
git -C "$WS" status --short | head -10
[ -d "$WS/.sortie" ] && for m in status model scm.json; do [ -f "$WS/.sortie/$m" ] && echo ".sortie/$m: $(cat "$WS/.sortie/$m")"; done
[ -n "$f" ] || { echo "== no transcript yet"; exit 0; }
echo "== transcript $f"

render() {
python3 -c '
import json,sys
for line in sys.stdin:
    try: d=json.loads(line)
    except Exception: continue
    m=d.get("message") or {}; c=m.get("content"); t=d.get("type")
    if t=="assistant" and isinstance(c,list):
        for b in c:
            if b.get("type")=="text" and b["text"].strip():
                print("\033[1m▌\033[0m "+b["text"].strip().replace("\n","\n  ")[:1200])
            elif b.get("type")=="tool_use":
                i=b.get("input",{}); s=i.get("command") or i.get("file_path") or i.get("pattern") or json.dumps(i)
                print("\033[36m⚙ %s\033[0m %s" % (b["name"], str(s).replace("\n"," ⏎ ")[:220]))
    elif t=="user" and isinstance(c,list):
        for b in c:
            if b.get("type")=="tool_result":
                out=b.get("content"); 
                if isinstance(out,list): out=" ".join(x.get("text","") for x in out if isinstance(x,dict))
                out=str(out or "").strip().replace("\n"," ⏎ ")
                if out: print("  ↳ "+out[:160])
'
}
if [ "${1:-}" = "-f" ]; then tail -n 40 -f "$f" | render
else tail -n "$(( ${1:-12} * 3 ))" "$f" | render | tail -n "$(( ${1:-12} * 2 ))"; fi
