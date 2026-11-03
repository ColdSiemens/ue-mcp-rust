# ue-mcp-rs: multi-agent Rust server

A Rust MCP server for Unreal Engine 5, built so several AI agents can drive one editor at the same time. It talks to this repository's `UE_MCP_Bridge` editor plugin, exposes the same tool surface as the Node server (28 categories, ~2,000 actions), and replaces only the Node.js part.

## Why the Node server struggles with many agents

| Problem | Cause | What this fork does |
|---|---|---|
| Agents wait on each other | The Node daemon sends every agent's calls down **one** socket, and the stock plugin answers one request per socket at a time. A 5-minute landscape erosion blocks everyone. | The Rust server keeps a pool of sockets and sends each call on an idle one: 4 sockets means 4 calls in flight even with a stock plugin. This fork's plugin also runs many requests per socket. |
| Slow even for tiny calls | Handlers run on the game thread, and an unfocused editor throttles its frame rate ("Use Less CPU when in Background"). Agents always drive an unfocused editor. | This fork's plugin turns throttling off while a client is connected. |
| Large requests crash or stall | The plugin closes the socket on messages over 64 MiB, which kills every call in flight on it. | The server refuses oversize requests itself with a clear error, so the socket survives. Answers up to 512 MiB are accepted. |
| Locks don't separate agents | The Node server's lock owner is per editor, so all agents count as the same owner. | Each MCP session, or each `agent` name, is its own owner. A mutation of an asset someone else locked is refused immediately. |
| Huge tool list in every agent's context | Full descriptions are ~50k tokens. | `--compact`, `--no-epic` and `--tools` cut it to 2k–15k tokens. |

One limit can't be removed: Unreal runs editor operations on its single game thread, so handlers still execute one after another. What changes is that no agent queues behind another agent's socket, and the editor ticks at full speed.

## Setup

**1. Install the editor plugin from this fork.** The plugin in [`plugin/ue_mcp_bridge`](../plugin/ue_mcp_bridge) adds concurrent requests, no background throttling, and builds on UE 5.5.4 through 5.8 (see [PLUGIN-PATCH.md](PLUGIN-PATCH.md)). Install it into a project; any plugin already there is moved to `Saved/` as a backup:

```powershell
.\rust\install-plugin.ps1 -Project C:\Path\To\MyGame\MyGame.uproject
```

Then rebuild the project's editor target; the script says how for C++ and for Blueprint-only projects. It has been compiled and live-tested on UE 5.5.4 and 5.8.2. The Rust server also works with a stock plugin, just with less concurrency.

**2. Get the server.** No Rust needed: each `rust-v*` release on this fork's Releases page carries prebuilt binaries for Windows, macOS and Linux. On Windows, this downloads the newest one and registers it for your project:

```powershell
.\rust\install.ps1 -Project C:\Path\To\MyGame.uproject
```

It installs to `%LOCALAPPDATA%\ue-mcp-rs\ue-mcp-rs.exe`; run it again to update. To build from source instead: `cd rust` then `cargo build --release`, which gives `rust/target/release/ue-mcp-rs.exe`.

**3. Register it with Claude Code** (`install.ps1 -Project` already did this). Remove any Node `ue-mcp` entry first so agents don't see every tool twice.

Option A, stdio: one Claude Code session and its subagents.

```bash
claude mcp add ue -- "<repo>\rust\target\release\ue-mcp-rs.exe" "C:\Path\To\MyGame.uproject" --compact
```

Option B, HTTP hub: several Claude Code sessions or terminals sharing one editor. Start the hub once:

```bash
"<repo>\rust\target\release\ue-mcp-rs.exe" "C:\Path\To\MyGame.uproject" --http 127.0.0.1:7878
```

Then point each session at it, optionally scoped:

```bash
claude mcp add --transport http ue "http://127.0.0.1:7878/mcp?agent=lighting&compact=1&tools=level,material"
```

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
- **Not ported** (removed from the advertised surface; the Node server still has them):
  - checkpoints
  - flows and feedback
  - npm plugin introspection
  - multi-editor routing
  - C++ source, config and engine-index tools (agents have their own file and shell tools for these)

## Engine versions

The plugin builds on UE 5.5 through 5.8. Code that needs a newer engine API is behind a version check, and a handler whose API is missing is not registered at all on that engine. The server reads the running plugin's handler list and engine version when it connects, then:

- hides actions the editor can't run from `tools/list`, so agents aren't offered them;
- refuses a call to one with a message naming the engine version, without sending anything to the editor;
- reports `availableInRunningEditor` in `project(describe_action)` and `engineVersion` in `project(get_status)`.

`--show-unregistered` turns the hiding and refusing off.

## Antivirus note

Some antivirus web shields (seen with Avast) inspect localhost HTTP and reset responses of a few MB or less, even with the project folder excluded. A raw-TCP control test showed it isn't the server. If HTTP mode drops large answers, add `http://127.0.0.1:7878/*` to the shield's exceptions or use stdio mode. The test suite runs its large-payload cases over stdio for this reason.

## Development

```bash
cd rust
cargo test                # 18 unit + 8 end-to-end tests against a fake editor bridge
cargo test -- --ignored   # plus the multi-MB HTTP test (fails under such an antivirus)
```

`surface/surface.json` is generated from this repository's TypeScript sources without running any of them. Regenerate it after merging upstream:

```bash
cd rust
python tools/extract_surface.py .. surface/surface.json
```

Current surface: upstream commit `cfa15afb` (1.4.12-beta.1).

## License

MIT, like the rest of this repository: see [../LICENSE](../LICENSE). Upstream is [db-lyon/ue-mcp](https://github.com/db-lyon/ue-mcp).
