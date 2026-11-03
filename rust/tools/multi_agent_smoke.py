"""Multi-agent smoke test: three agents build game pieces in one editor at once.

The agents make the same tool calls a Claude agent would, with no thinking
time between them, so this loads the bridge harder than real agents do:

  menu       a main-menu UMG widget (title, Play/Options/Quit buttons)
  inventory  an ActorComponent Blueprint: item array, slots, Add/Remove/HasItem
  items      an interactable item Actor Blueprint, three instances placed in the level

An observer streams reads alongside, and the items agent checks that a mutation
of the menu agent's locked widget is refused. Reports per-agent latency and errors.

Transports:
  stdio (default)  one shared connection, as Claude Code subagents share one; each
                   agent names itself with the per-call `agent` argument
  http             one MCP session per agent on a Streamable HTTP hub

Usage (stdlib only, Python 3.8+):
  python rust/tools/multi_agent_smoke.py --exe <ue-mcp-rs.exe> --project <Game.uproject> [--start-editor] [--transport http]
"""
import argparse
import json
import statistics
import subprocess
import threading
import time
import urllib.error
import urllib.request

p = argparse.ArgumentParser()
p.add_argument("--exe", required=True)
p.add_argument("--project", required=True)
p.add_argument("--transport", choices=["stdio", "http"], default="stdio")
p.add_argument("--port", type=int, default=7891)
p.add_argument("--connections", type=int, default=4)
p.add_argument("--start-editor", action="store_true", help="launch the editor through the server and stop it afterwards")
p.add_argument("--hub-log", default="multi-agent-hub.log")
args = p.parse_args()

BASE = "/Game/MCP_MultiAgent/Run_" + time.strftime("%H%M%S")
INIT = {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "multi-agent-smoke"}}
HUB_ARGS = [args.exe, args.project, "--connections", str(args.connections),
            "--editor-arg=-ini:EditorSettings:[/Script/SourceCodeAccess.SourceCodeAccessSettings]:PreferredAccessor=NullSourceCodeAccessor"]


class StdioTransport:
    """One server process; requests from many threads, matched to answers by id."""

    def __init__(self):
        self.proc = subprocess.Popen(HUB_ARGS, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(args.hub_log, "w"))
        self.lock, self.next_id, self.waiters = threading.Lock(), 0, {}
        threading.Thread(target=self._read, daemon=True).start()
        self.request("initialize", INIT)

    def _read(self):
        for line in self.proc.stdout:
            msg = json.loads(line)
            waiter = self.waiters.pop(msg.get("id"), None)
            if waiter:
                waiter[1].append(msg)
                waiter[0].set()

    def request(self, method, params):
        done, box = threading.Event(), []
        with self.lock:
            self.next_id += 1
            self.waiters[self.next_id] = (done, box)
            frame = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}
            self.proc.stdin.write((json.dumps(frame) + "\n").encode())
            self.proc.stdin.flush()
        if not done.wait(900):
            raise TimeoutError(method)
        return box[0]

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)


class HttpSession:
    """One MCP session on the HTTP hub."""

    def __init__(self, agent):
        self.id = None
        self.url = "http://127.0.0.1:%d/mcp" % args.port
        self.request("initialize", INIT, query="?agent=%s&compact=1" % agent)

    def request(self, method, params, query=""):
        req = urllib.request.Request(self.url + query, method="POST",
                                     data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
                                     headers={"Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
        if self.id:
            req.add_header("Mcp-Session-Id", self.id)
        with urllib.request.urlopen(req, timeout=900) as resp:
            self.id = self.id or resp.headers.get("Mcp-Session-Id")
            return json.loads(resp.read())


class Agent:
    def __init__(self, name, transport):
        self.name, self.calls, self.result = name, [], ""
        self.transport = transport if args.transport == "stdio" else HttpSession(name)

    def call(self, tool, action, expect_error=False, **a):
        if args.transport == "stdio":
            a["agent"] = self.name  # per-call lock identity on a shared connection
        t0 = time.perf_counter()
        try:
            r = self.transport.request("tools/call", {"name": tool, "arguments": {"action": action, "args": a}})["result"]
            err, text = r["isError"], r["content"][0]["text"]
        except (urllib.error.URLError, OSError, KeyError, TimeoutError, ValueError) as e:
            err, text = True, "transport: %r" % e
        ms = (time.perf_counter() - t0) * 1000
        ok = err == expect_error
        self.calls.append((tool + "." + action, ms, ok))
        if not ok:
            print("  [%s] UNEXPECTED %s in %s.%s: %s" % (self.name, "error" if err else "success", tool, action, text[:300]), flush=True)
        return text


def menu(s, release_lock, lock_held):
    w = BASE + "/WBP_MainMenu"
    s.call("asset", "lock", assetPath=w)
    lock_held.set()
    s.call("widget", "create", assetPath=w)
    s.call("widget", "add_widget", assetPath=w, widgetClass="CanvasPanel", widgetName="Root")
    s.call("widget", "add_widget", assetPath=w, widgetClass="VerticalBox", widgetName="MenuBox", parentWidgetName="Root")
    s.call("widget", "add_widget", assetPath=w, widgetClass="TextBlock", widgetName="Title", parentWidgetName="MenuBox")
    s.call("widget", "set_property", assetPath=w, widgetName="Title", propertyName="Text", value='INVTEXT("MAIN MENU")')
    for b in ("Play", "Options", "Quit"):
        s.call("widget", "add_widget", assetPath=w, widgetClass="Button", widgetName=b + "Button", parentWidgetName="MenuBox")
        s.call("widget", "add_widget", assetPath=w, widgetClass="TextBlock", widgetName=b + "Label", parentWidgetName=b + "Button")
        s.call("widget", "set_property", assetPath=w, widgetName=b + "Label", propertyName="Text", value='INVTEXT("%s")' % b)
    s.call("blueprint", "compile", assetPath=w)
    s.call("asset", "save", assetPath=w)
    tree = s.call("widget", "read_tree", assetPath=w)
    found = [n for n in ("Root", "MenuBox", "Title", "PlayButton", "OptionsButton", "QuitButton") if '"%s"' % n in tree]
    s.result = "tree has " + ", ".join(found)
    release_lock.wait(timeout=300)  # hold the lock so the items agent's conflict check is deterministic
    s.call("asset", "unlock", assetPath=w)


def inventory(s):
    c = BASE + "/BP_InventoryComponent"
    s.call("asset", "lock", assetPath=c)
    s.call("blueprint", "create", assetPath=c, parentClass="ActorComponent")
    s.call("blueprint", "add_variable", assetPath=c, name="Items", varType="name[]", category="Inventory")
    s.call("blueprint", "add_variable", assetPath=c, name="MaxSlots", varType="int", category="Inventory", instanceEditable=True)
    s.call("blueprint", "add_variable", assetPath=c, name="bIsFull", varType="bool", category="Inventory")
    for f in ("AddItem", "RemoveItem", "HasItem"):
        s.call("blueprint", "create_function", assetPath=c, functionName=f)
    s.call("blueprint", "compile", assetPath=c)
    s.call("asset", "save", assetPath=c)
    v = s.call("blueprint", "list_variables", assetPath=c)
    f = s.call("blueprint", "list_functions", assetPath=c)
    s.result = "variables ok=%s, functions ok=%s" % (all(n in v for n in ("Items", "MaxSlots", "bIsFull")),
                                                     all(n in f for n in ("AddItem", "RemoveItem", "HasItem")))
    s.call("asset", "unlock", assetPath=c)


def items(s, menu_lock_held):
    a = BASE + "/BP_InteractableItem"
    s.call("asset", "lock", assetPath=a)
    s.call("blueprint", "create", assetPath=a, parentClass="Actor")
    s.call("blueprint", "add_component", assetPath=a, componentClass="StaticMeshComponent", componentName="ItemMesh")
    s.call("blueprint", "add_component", assetPath=a, componentClass="SphereComponent", componentName="InteractionSphere")
    s.call("blueprint", "add_variable", assetPath=a, name="ItemName", varType="name", instanceEditable=True)
    s.call("blueprint", "add_variable", assetPath=a, name="Quantity", varType="int", instanceEditable=True)
    s.call("blueprint", "add_variable", assetPath=a, name="bCanPickUp", varType="bool")
    for f in ("Interact", "OnPickedUp"):
        s.call("blueprint", "create_function", assetPath=a, functionName=f)
    s.call("blueprint", "compile", assetPath=a)
    s.call("asset", "save", assetPath=a)
    for i in range(3):
        s.call("level", "place_actor", actorClass=a, label="MA_Item_%d" % i, location={"x": i * 200.0, "y": 0, "z": 100})
    menu_lock_held.wait(timeout=120)
    conflict = s.call("widget", "add_widget", expect_error=True, assetPath=BASE + "/WBP_MainMenu",
                      widgetClass="TextBlock", widgetName="Intruder", parentWidgetName="MenuBox")
    outliner = s.call("level", "get_outliner", nameFilter="MA_Item_", limit=10)
    try:
        placed = sum(1 for x in json.loads(outliner).get("actors", []) if str(x.get("label", "")).startswith("MA_Item_"))
    except ValueError:
        placed = "?"
    s.result = "placed %s in level, lock conflict refused=%s" % (placed, "menu" in conflict)
    s.call("asset", "unlock", assetPath=a)


def observer(s, stop):
    while not stop.is_set():
        s.call("level", "get_outliner", limit=3)
        s.call("editor", "get_engine_state", probeWindows=False)
        s.call("asset", "list", directory=BASE, limit=20)


def stats(calls):
    ms = sorted(c[1] for c in calls)
    p95 = ms[min(len(ms) - 1, int(0.95 * len(ms)))]
    return "%4d calls  p50 %7.1f ms  p95 %7.1f ms  max %7.1f ms  errors %d" % (
        len(ms), statistics.median(ms), p95, ms[-1], sum(not c[2] for c in calls))


hub = None
if args.transport == "http":
    hub = subprocess.Popen(HUB_ARGS + ["--http", "127.0.0.1:%d" % args.port], stdout=subprocess.DEVNULL, stderr=open(args.hub_log, "w"))
    time.sleep(1.0)
    shared = None
else:
    shared = StdioTransport()
try:
    control = Agent("control", shared)
    if args.start_editor:
        print("starting editor:", control.call("editor", "start_editor", timeout=1800)[:160], flush=True)
    ed = json.loads(control.call("project", "get_status"))["editor"]
    print("editor %s concurrentDispatch=%s sockets=%d transport=%s; assets under %s" % (
        ed.get("engineVersion"), ed["concurrentDispatch"], len(ed["connections"]), args.transport, BASE), flush=True)

    agents = {n: Agent(n, shared) for n in ("menu", "inventory", "items", "observer")}
    release_menu_lock, menu_lock_held, stop = threading.Event(), threading.Event(), threading.Event()
    builders = [
        threading.Thread(target=menu, args=(agents["menu"], release_menu_lock, menu_lock_held)),
        threading.Thread(target=inventory, args=(agents["inventory"],)),
        threading.Thread(target=items, args=(agents["items"], menu_lock_held)),
    ]
    watch = threading.Thread(target=observer, args=(agents["observer"], stop))
    t0 = time.perf_counter()
    for t in builders + [watch]:
        t.start()
    builders[1].join()
    builders[2].join()
    release_menu_lock.set()
    builders[0].join()
    stop.set()
    watch.join()
    wall = time.perf_counter() - t0

    print("\nall agents done in %.1fs wall clock" % wall)
    busy = 0.0
    for n, a in agents.items():
        if n != "observer":
            busy += sum(c[1] for c in a.calls) / 1000
        print("  %-9s %s  %s" % (n, stats(a.calls), a.result))
    print("  %-9s %s" % ("total", stats([c for a in agents.values() for c in a.calls])))
    print("  builder agents' summed call time %.1fs in %.1fs wall: overlap x%.1f" % (busy, wall, busy / wall))
    health = json.loads(control.call("project", "get_status"))
    print("  calls answered per editor socket:", [c["answered"] for c in health["editor"]["connections"]],
          "| locks left:", len(health["locks"]))
    if args.start_editor:
        print("stopping editor:", control.call("editor", "stop_editor")[:120])
finally:
    if shared:
        shared.close()
    if hub:
        hub.terminate()
