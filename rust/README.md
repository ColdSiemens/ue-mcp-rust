# ue-mcp-rs: multi-agent Rust server

A Rust MCP server for Unreal Engine 5, built so several AI agents can drive one editor at the same time. It talks to this repository's `UE_MCP_Bridge` editor plugin and exposes the same tool surface as upstream's Node server (28 categories, ~2,000 actions).

Installation, requirements and the comparison with upstream are in the [main README](../README.md). This page is the server reference: transports, multi-agent details, options, and what was ported.

## Transports

- **stdio** (default): one Claude Code session and all its subagents share one connection. `ue-mcp-rs.exe <Game.uproject> --compact`, or no project argument when started from the game's folder.
- **Streamable HTTP**: several Claude Code sessions or terminals share one hub. Start it once with `ue-mcp-rs.exe <Game.uproject> --http 127.0.0.1:7878`, then add each session with `claude mcp add --transport http ue "http://127.0.0.1:7878/mcp?agent=lighting&compact=1&tools=level,material"`. See the antivirus note below.

## Running multiple agents

- **Name each agent.** Over HTTP, use `?agent=<name>` in the URL. Subagents of one Claude Code session share a single connection, so tell each one to pass `agent:"<name>"` inside `args` on every call. The server instructions already tell models to do this.
- **Lock before multi-step edits.** Call `asset(action="lock", args={assetPath, agent})`, do the work, then `asset(action="unlock")`. Another agent's mutation of that asset is refused with the holder's name. Locks expire after `ttlSeconds` (default 300), and an HTTP session's locks are released when it closes.
- **Give agents disjoint areas** (their own folders, levels or sublevels). The game thread serializes their work, but the work doesn't collide.
- **Prefer batch actions** (`batch_set_properties`, `spawn_actors_batch`, `niagara(batch)`, `blueprint(author)`). One game-thread trip beats twenty.
- A timed-out mutation may still have landed, so read the state before retrying. Pass `timeoutMs` for long operations.

Example subagent brief:

> You are the lighting agent. Use the `ue` tools and pass `agent:"lighting"` in args on every call. Work only in /Game/Lighting and the Arena_Lighting sublevel. Lock an asset before editing it and unlock it when you're done.

## Options

| Flag | Default | |
|---|---|---|
| `PROJECT` | current dir | `.uproject` file or its folder. Used to find the editor's port. |
| `--http ADDR` | stdio | Serve Streamable HTTP, e.g. `127.0.0.1:7878`. |
| `--connections N` | 4 | Sockets to the editor. With a stock plugin, N = calls in flight. |
| `--compact` | off | Action names only in descriptions; `project(describe_action)` gives the details. |
| `--no-epic` | off | Hide the 830 `epic_*` wrappers of Unreal 5.8's own toolsets. |
| `--tools a,b` | all | Only these categories (`project` is always included). |
| `--bridge-port P` / `UE_MCP_PORT` | auto | Skip port discovery. |
| `--engine-root DIR` / `UE_ENGINE_ROOT` | auto | Engine folder for `start_editor` and builds. Default: the `.uproject`'s EngineAssociation (launcher version, registered source build, or path). `UE_EDITOR_PATH` names the editor exe directly. |
| `--agent NAME` / `UE_MCP_AGENT` | random | Lock owner for the stdio session. |
| `--no-lock-enforcement` | off | Allow mutating assets other agents locked. |
| `--show-unregistered` | off | Advertise actions the running plugin doesn't register. |
| `UE_MCP_LOG` | `info` | Log filter (stderr). |

HTTP query parameters per session: `agent`, `tools`, `compact=1`, `epic=0`. `GET /health` returns the server's status: sockets, in-flight calls, sessions and locks.

Tool-list size, as measured:

| View | Size | ~Tokens |
|---|---|---|
| full | 197 KB | 50k |
| `--compact` | 60 KB | 15k |
| `--compact --no-epic` | 40 KB | 10k |
| `--compact --no-epic --tools level,blueprint,material` | 8 KB | 2k |

## What was ported

- **All bridge actions (2,053)** are forwarded unchanged, including the `epic_*` envelope (asset refs expand, `input`/`inputJson`).
- **Server-side actions:**
  - `asset`: `lock`, `unlock`, `unlock_all`, `search`, `migrate`
  - `blueprint(author)`, `niagara(batch)`, `pcg(wait_for_generation)` (and `wait:true` on `pcg(execute/force_regenerate)`)
  - `editor`: `get_engine_state`, `execute_python` (ungated), `play_in_editor_ignore_blueprint_errors`
  - `project`: `get_status`, `get_info`, `describe_action`, `list_available_actions`, `search_tools`, `read_config`, `search_config`, `list_config_tags`, `build`
  - `editor`: `start_editor` (launches detached, waits for the bridge to report ready), `stop_editor` (the editor quits itself and raises its own save prompt), `restart_editor`, `request_editor_shutdown`, `build_project` (UnrealBuildTool, refused while the editor runs)
- **New in this fork:** `blueprint(auto_layout)` and `blueprint(restore_layout)`. See below.
- **Not ported** (removed from the advertised surface; the Node server still has them):
  - checkpoints
  - flows and feedback
  - npm plugin introspection
  - multi-editor routing
  - C++ source, config and engine-index tools (agents have their own file and shell tools for these)

## Blueprint graph layout

Agents building Blueprints either leave a heap of nodes or spend tokens working out positions as they go. Here they don't position nodes at all: they add and wire nodes anywhere, then call `blueprint(action="auto_layout", args={assetPath})` once. The server instructions say so.

The layout is a port of the bp-graph-layout skill's row layout:
- one row per event or function entry;
- exec nodes on one straight line;
- the pure nodes feeding a node tucked under the wire just before it;
- extra exec outputs (Branch False, Cast Failed, loop Completed) on sub-rows below;
- merges placed right of their last source.

Parity tests check the Rust port against the original JS output on captured graphs.

- **Which graphs.** With no graph named, it lays out the graphs this agent added nodes to (`add_node`, `import_nodes_t3d`, `connect_pins`), else `EventGraph`. `graphName`, `graphs` or `all:true` choose explicitly. `dryRun:true` computes without moving.
- **What it does.** It moves positions only and compiles nothing; `save:true` saves the asset. It is refused while Play In Editor runs, and it honours other agents' locks.
- **Moves and undo.** This fork's plugin moves the whole graph in one call (`set_node_positions`), as one undo step. With a stock plugin it falls back to one call per node, with no undo. Measured with 19 nodes and the graph open: 0.10 s against 0.66 s per node on 5.8, 0.13 s against 0.91 s on 5.5.4.
- **Reports.** Every run writes `Saved/BPLayout/<Asset>.<Graph>.layout.json` (every node's old and new position) and an SVG sketch. `blueprint(action="restore_layout", args={report})` puts the graph back exactly; one Ctrl+Z also does.
- **Limits** (from the original):
  - node sizes are estimated from UE's default style;
  - comment boxes are parked below the rows rather than re-wrapped;
  - a node fed from several events' rows can leave a long backward wire;
  - K2 graphs only, unless `force:true`.

## Engine versions

The plugin builds on UE 5.5 through 5.8. Code that needs a newer engine API is behind a version check, and a handler whose API is missing is not registered at all on that engine. The server reads the running plugin's handler list and engine version when it connects, then:

- hides actions the editor can't run from `tools/list`, so agents aren't offered them;
- refuses a call to one with a message naming the engine version, without sending anything to the editor;
- reports `availableInRunningEditor` in `project(describe_action)` and `engineVersion` in `project(get_status)`.

`--show-unregistered` turns the hiding and refusing off.

## Antivirus note

Some antivirus web shields (seen with Avast) proxy localhost HTTP, and a project-folder exclusion doesn't cover that. On the machine this was built on, the shield:
- **rewrote responses**: it replaced `Content-Length` with `Transfer-Encoding: chunked` without chunking the body, so strict HTTP clients fail on every reply;
- **reset multi-MB answers**.

Control tests with a bare TCP server, no ue-mcp-rs code involved, showed the same. Either exclude `127.0.0.1` / the hub's port in the shield, or use **stdio mode**, which isn't affected and is what Claude Code subagents share anyway. The test suite runs its large-payload cases over stdio for this reason.

## Development

```bash
cd rust
cargo test                # 18 unit + 8 end-to-end tests against a fake editor bridge
cargo test -- --ignored   # plus the multi-MB HTTP test (fails under such an antivirus)
```

Multi-agent smoke test against a real editor (Python stdlib only). Three agents run at once while an observer streams reads:
- `menu` builds a UMG main menu;
- `inventory` builds an inventory ActorComponent;
- `items` builds an interactable item actor and places it in the level.

The test also checks that one agent's lock refuses another agent's edit:

```bash
python rust/tools/multi_agent_smoke.py --exe rust/target/release/ue-mcp-rs.exe --project tests/ue_mcp/ue_mcp.uproject --start-editor
```

On UE 5.8.2 it ran 76 calls with 0 errors in 2.2 s wall clock (p50 96 ms, p95 470 ms). The agents' work overlapped 2.8× and was spread evenly over the 4 editor sockets. Assets land under `/Game/MCP_MultiAgent/Run_<time>`. `--transport http` runs one MCP session per agent instead.

`surface/surface.json` is generated from this repository's TypeScript sources without running any of them. Regenerate it after merging upstream:

```bash
cd rust
python tools/extract_surface.py .. surface/surface.json
```

Current surface: upstream commit `cfa15afb` (1.4.12-beta.1).

## License

MIT, like the rest of this repository: see [../LICENSE](../LICENSE). Upstream is [db-lyon/ue-mcp](https://github.com/db-lyon/ue-mcp).
