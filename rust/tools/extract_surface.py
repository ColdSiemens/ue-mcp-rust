"""Static extraction of the ue-mcp action surface (no code from the repo is executed).

Reads src/tools/**/*.ts and tests/golden/editor-connected.json and writes a
surface.json the Rust hub embeds: per tool, the advertised schema/description
and, per action, how it dispatches (bridge method, epic envelope, or local).

Usage: python -I extract_surface.py <repo-root> <out.json>
"""
import json
import os
import re
import sys

repo, out_path = sys.argv[1], sys.argv[2]
tools_dir = os.path.join(repo, "src", "tools")


def lex_strip(src):
    """Blank out comments so they never match, keeping offsets stable."""
    out = list(src)
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c in "\"'`":
            q = c
            i += 1
            while i < n and src[i] != q:
                i += 2 if src[i] == "\\" else 1
            i += 1
        elif src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            for k in range(i, j):
                out[k] = " "
            i = j
        elif src.startswith("/*", i):
            j = src.find("*/", i + 2)
            j = n if j < 0 else j + 2
            for k in range(i, j):
                if out[k] != "\n":
                    out[k] = " "
            i = j
        else:
            i += 1
    return "".join(out)


def call_args(src, open_idx):
    """Split the argument list of the call whose '(' is at open_idx."""
    depth, i, n = 0, open_idx, len(src)
    args, start = [], open_idx + 1
    while i < n:
        c = src[i]
        if c in "\"'`":
            q = c
            i += 1
            while i < n and src[i] != q:
                i += 2 if src[i] == "\\" else 1
        elif c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                args.append(src[start:i].strip())
                return args, i
        elif c == "," and depth == 1:
            args.append(src[start:i].strip())
            start = i + 1
        i += 1
    return args, n


STR = re.compile(r'"((?:[^"\\]|\\.)*)"')


def string_value(expr):
    """Evaluate a string literal or a '+' concatenation of them; None otherwise."""
    parts = [p.strip() for p in re.split(r'"\s*\+\s*"', expr)]
    if len(parts) > 1:
        expr = '"' + "".join(p.strip('"') for p in parts) + '"'
    m = STR.fullmatch(expr.strip())
    return json.loads('"' + m.group(1) + '"') if m else None


BP_CALL = re.compile(r"(?:^|[\s,{(])(?P<key>[A-Za-z_]\w*)\s*:\s*(?P<fn>\w*[sS]pecBp|bp)\s*\(", re.M)
EPIC_ENTRY = re.compile(
    r"^\s{2}(?P<key>epic_\w+):\s*\{\s*epicSchema:\s*(?P<schema>S_\w+),\s*"
    r"epicTool:\s*\{\s*toolset:\s*\"(?P<toolset>[^\"]+)\",\s*name:\s*\"(?P<name>[^\"]+)\"\s*\},\s*"
    r"\.\.\.bp\(\s*\"(?P<effect>\w+)\",\s*\"(?P<desc>(?:[^\"\\]|\\.)*)\"",
    re.M,
)
SCHEMA_CONST = re.compile(r"^const (S_\w+) = (\{.*\}) as const;$", re.M)


OBJ_ENTRY = re.compile(r"(?:^|[\s,{(])(?P<key>[A-Za-z_]\w*)\s*:\s*\{", re.M)
SPREAD_BP = re.compile(r"^\s*\.\.\.(?P<fn>\w*[sS]pecBp|bp)\s*\(")
TIMEOUT = re.compile(r"(?:^|[\s,{])timeoutMs\s*:\s*([\d_]+)")


def parse_bp(src, open_idx, fn):
    args, _ = call_args(src, open_idx)
    strs = [string_value(a) for a in args]
    effect = strs[0] if strs else None
    # bp(effect, bridge[, map]) / bp(effect, desc, bridge[, map]) / specBp(effect, desc, bridge)
    lits = [s for s in strs[1:] if s is not None]
    if not lits:
        return None
    bridge = lits[-1] if len(lits) <= 2 else lits[1]
    if not re.fullmatch(r"[a-z][a-z0-9_]*", bridge or ""):
        return None
    has_map = fn == "bp" and len(args) > (1 + len(lits))
    spec = {"kind": "bridge", "bridge": bridge, "effect": effect, "mapParams": has_map}
    if len(lits) >= 2:
        spec["summary"] = lits[0]
    return spec


def scan_file(path):
    raw = open(path, encoding="utf-8").read()
    src = lex_strip(raw)
    found = {}
    for m in BP_CALL.finditer(src):
        spec = parse_bp(src, m.end() - 1, m.group("fn"))
        if spec:
            found[m.group("key")] = spec
    # { ...specBp(...), timeoutMs: N, destructive: true } and { kind: "handler", ... }
    for m in OBJ_ENTRY.finditer(src):
        key = m.group("key")
        _, close = call_args(src, m.end() - 1)
        body = src[m.end():close]
        top = body.lstrip()
        sm = SPREAD_BP.match(body)
        if sm:
            spec = parse_bp(src, m.end() + sm.end() - 1, sm.group("fn"))
            if not spec:
                continue
            tm = TIMEOUT.search(body)
            if tm:
                spec["timeoutMs"] = int(tm.group(1).replace("_", ""))
            if re.search(r"\bfollowUp\s*:", body):
                spec["followUp"] = True
            found[key] = spec
        elif top.startswith('kind: "handler"') and key not in found:
            found[key] = {"kind": "local"}
    return found


def scan_epic(path):
    src = open(path, encoding="utf-8").read()
    schemas = {m.group(1): json.loads(m.group(2)) for m in SCHEMA_CONST.finditer(src)}
    found = {}
    for m in EPIC_ENTRY.finditer(src):
        found[m.group("key")] = {
            "kind": "epic",
            "bridge": "epic_call_tool",
            "effect": m.group("effect"),
            "summary": json.loads('"' + m.group("desc") + '"'),
            "toolset": m.group("toolset"),
            "tool": m.group("name"),
            "schema": schemas.get(m.group("schema"), {}),
        }
    return found


golden = json.load(open(os.path.join(repo, "tests", "golden", "editor-connected.json"), encoding="utf-8"))

# Every action key found anywhere, by file stem (tool files are named after their tool).
by_file = {}
for root, _, files in os.walk(tools_dir):
    for f in files:
        if not f.endswith(".ts"):
            continue
        p = os.path.join(root, f)
        rel = os.path.relpath(p, tools_dir).replace("\\", "/")
        by_file[rel] = scan_epic(p) if rel.startswith("epic/") and rel.endswith(".generated.ts") else scan_file(p)

tools_out = []
report = []
for t in golden["tools"]:
    name = t["name"]
    enum = []
    act = t["inputSchema"]["properties"].get("action", {})
    for branch in act.get("anyOf", [act]):
        enum += branch.get("enum", [])
    # Only the tool's own file(s) and its epic file: action names like
    # get_status or list recur across categories with different methods.
    order = [f"{name}.ts", f"epic/{name}.generated.ts"]
    if name == "project":
        order += [f"project/{x}" for x in sorted(os.listdir(os.path.join(tools_dir, "project")))]
    actions, missing = {}, []
    for a in enum:
        hits = [by_file[f][a] for f in order if f in by_file and a in by_file[f]]
        spec = next((h for h in hits if h["kind"] != "local"), hits[0] if hits else None)
        if spec is None or spec["kind"] == "local":
            missing.append(a)
            actions[a] = {"kind": "local"}
        else:
            actions[a] = spec
    report.append((name, len(enum), len(missing), missing))
    tools_out.append({
        "name": name,
        "description": t["description"],
        "inputSchema": t["inputSchema"],
        "actions": actions,
    })

# The C++ handlers' recorded parameter contracts, for describe_action.
specs_path = os.path.join(repo, "tests", "golden", "handler-specs.json")
handler_specs = {}
if os.path.exists(specs_path):
    for method, spec in json.load(open(specs_path, encoding="utf-8")).get("handlers", {}).items():
        handler_specs[method] = {k: v for k, v in spec.items() if k != "category"}

# Server-side game-thread timeouts the C++ handlers register (mirrored in TS).
timeouts_src = open(os.path.join(repo, "src", "bridge", "bridge-timeouts.ts"), encoding="utf-8").read()
table = timeouts_src[timeouts_src.find("REGISTERED_HANDLER_TIMEOUT_SECONDS"):]
table = table[:table.find("};")]
handler_timeouts = {m.group(1): int(m.group(2)) for m in re.finditer(r"^\s+(\w+):\s*(\d+),", table, re.M)}

json.dump({"tools": tools_out, "handlerSpecs": handler_specs, "handlerTimeoutSeconds": handler_timeouts},
          open(out_path, "w", encoding="utf-8"), ensure_ascii=False, separators=(",", ":"))

total = sum(r[1] for r in report)
local = sum(r[2] for r in report)
print(f"tools={len(report)} actions={total} unmapped={local}")
for name, n, k, miss in report:
    if k:
        print(f"  {name}: {k}/{n} unmapped: {', '.join(miss)}")
