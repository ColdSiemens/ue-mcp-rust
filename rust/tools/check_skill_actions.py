"""Check that every tool(action=...) the agent skills cite exists on the server.

Usage: python check_skill_actions.py <ue-mcp-rs executable> <skills folder>

Starts the server with no editor, reads its tool list, and prints, per skill,
the cited actions it doesn't offer. Exits 1 if any are missing.
"""
import json
import os
import re
import subprocess
import sys

exe, skills = os.path.abspath(sys.argv[1]), sys.argv[2]

server = subprocess.Popen([exe, "--bridge-port", "1", "--connect-wait", "1"],
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
requests = [("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "skill-check"}}),
            ("tools/list", {})]
for i, (method, params) in enumerate(requests):
    server.stdin.write((json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n").encode())
    server.stdin.flush()
server.stdout.readline()
tools = json.loads(server.stdout.readline())["result"]["tools"]
server.kill()

surface = {t["name"]: set(t["inputSchema"]["properties"]["action"].get("enum", [])) for t in tools}
cite = re.compile(r"\b([a-z_]+)\(action\s*=\s*['\"]?([a-zA-Z_0-9]+)")

missing_total = 0
for name in sorted(os.listdir(skills)):
    path = os.path.join(skills, name, "SKILL.md")
    if not os.path.isfile(path):
        continue
    refs = sorted(set(cite.findall(open(path, encoding="utf-8").read())))
    missing = [f"{tool}.{action}" for tool, action in refs if action not in surface.get(tool, ())]
    missing_total += len(missing)
    print(f"{name}: {len(refs)} actions cited, {len(missing)} missing {', '.join(missing)}".rstrip())
sys.exit(1 if missing_total else 0)
