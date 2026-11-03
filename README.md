# UE-MCP, multi-agent edition

Control the Unreal Engine editor from AI agents, **several of them at once**. A fork of [db-lyon/ue-mcp](https://github.com/db-lyon/ue-mcp) with:

- **A Rust MCP server** ([`rust/`](rust/README.md)) in place of the Node one. One small `.exe` with no runtime, built so many agents can share one editor.
- **An improved editor plugin** ([`plugin/ue_mcp_bridge`](plugin/ue_mcp_bridge), [what changed](rust/PLUGIN-PATCH.md)):
  - it runs requests concurrently;
  - it stops the editor throttling itself while agents drive it from the background;
  - it builds on **UE 5.5 through 5.8**.

The same ~2,000 editor actions as upstream: Blueprints, UMG, levels, materials, animation, Niagara, PCG, landscape, audio, StateTree, GAS and more.

## Why this fork

Upstream sends every agent's calls through one connection, which the plugin answers one at a time, so agents queue behind each other. This fork:

| | Upstream | This fork |
|---|---|---|
| Agents in flight | 1 (one shared socket, answered in turn) | many: a socket pool, and each socket runs requests concurrently |
| Unfocused editor | throttles to a few FPS | full speed while a client is connected |
| Per-agent asset locks | one owner per editor | one per agent, shared by every session; another agent's mutation is refused |
| Oversized requests | close the socket and every call on it | refused with a clear error |
| UE 5.5.4 | doesn't compile | compiles and runs (verified) |
| Tool list in context | ~50k tokens | 2k–15k with `--compact`, `--tools`, `--no-epic` |
| Blueprint node layout | a fixed 360×200 grid, one node per call | readable rows in one call: [`auto_layout`](rust/README.md#blueprint-graph-layout), one undo step |

Measured with three agents building a UMG menu, an inventory component and an interactable item in one UE 5.8 editor at once: 76 calls, 0 errors, 2.2 s wall clock, p50 96 ms ([test script](rust/tools/multi_agent_smoke.py)).

Unreal still runs editor operations on its single game thread; what's gone is agents waiting on each other's sockets.

## Requirements

- Windows 10/11; the server also builds for macOS and Linux.
- Unreal Engine **5.5–5.8**: launcher install or source build. Verified on 5.5.4 and 5.8.2.
- Visual Studio 2022 with the C++ game-dev workload, to compile the plugin.
- [Claude Code](https://claude.com/claude-code), or any MCP client.

## Install

**1. Get this repository.** The fork's work is on the `rust-hub` branch:

```bash
git clone -b rust-hub https://github.com/ColdSiemens/ue-mcp-rust.git
```

**2. Install the plugin into your game.** Any `UE_MCP_Bridge` already there, stock or patched by hand, is moved to `Saved\` as a backup:

```powershell
.\rust\install-plugin.ps1 -Project D:\Games\MyGame\MyGame.uproject
```

It also copies the six `ue-mcp-*` agent skills (workflow, Blueprint, native C++, Epic routing, Niagara, animation) into the game's `.claude\skills\`. It replaces only those six folders, backs the old copies up to `Saved\`, and leaves your other skills alone. Pass `-SkipSkills` to install the plugin only.

Then compile it for your engine:
- **C++ project:** build the Editor target.
- **Blueprint-only project:** open the project and accept the rebuild prompt.

**3. Get the server.**
- **Prebuilt** (no Rust needed), once a `rust-v*` release is on the Releases page. This downloads it to `%LOCALAPPDATA%\ue-mcp-rs` and registers it with Claude Code for that project:
  ```powershell
  .\rust\install.ps1 -Project D:\Games\MyGame\MyGame.uproject
  ```
- **From source:** gives `rust\target\release\ue-mcp-rs.exe`.
  ```bash
  cd rust
  cargo build --release
  ```

**4. Register it with Claude Code** (step 3's `install.ps1` already did this). Use stdio:

```powershell
claude mcp add ue -- "$env:LOCALAPPDATA\ue-mcp-rs\ue-mcp-rs.exe" "D:\Games\MyGame\MyGame.uproject" --compact
```

For a team, commit a `.mcp.json` at the game's root, so everyone who installed the server gets it with no per-machine paths. The server finds the `.uproject` in its working folder:

```json
{ "mcpServers": { "ue": { "command": "${LOCALAPPDATA}/ue-mcp-rs/ue-mcp-rs.exe", "args": ["--compact"] } } }
```

Remove any old Node `ue-mcp` entry so agents don't see every tool twice.

**5. Check it.** Ask the agent to run `project(action="get_status")`. It reports the editor's engine version, `concurrentDispatch: true`, and the open connections. `editor(action="start_editor")` launches the editor if it isn't running.

## Running several agents

- **Each Claude Code session starts its own `ue-mcp-rs.exe`** (that's how stdio MCP servers work); its subagents share it. All of them talk to the same editor, and the editor plugin holds the locks, so a lock taken in one session stops writes from every other.
- **Subagents of one Claude Code session share one connection.** Tell each one to put `agent: "<its name>"` in the args of every call, so its locks are its own.
- **Separate Claude Code sessions** can share one HTTP hub, `ue-mcp-rs.exe <uproject> --http 127.0.0.1:7878`, with each session using `http://127.0.0.1:7878/mcp?agent=<name>`. See the antivirus note below first.
- **Lock before multi-step edits:** `asset(action="lock", args={assetPath})` … `asset(action="unlock")`.
- **Give each agent its own folder or sublevel, and prefer batch actions.** The game thread does one thing at a time.
- **Blueprints: don't let agents position nodes.** They should add and wire nodes anywhere, then call `blueprint(action="auto_layout", args={assetPath})` once.

Example subagent brief:

> You are the UI agent. Use the `ue` tools and pass `agent:"ui"` in the args of every call. Work only under /Game/UI. Lock a widget before editing it and unlock it when you're done.

All options, the per-session filters and the full list of what was ported are in [rust/README.md](rust/README.md).

## Engine versions

| Engine | Status |
|---|---|
| 5.8 | Compiled and live-tested (5.8.2) |
| 5.5 | Compiled and live-tested (5.5.4) |
| 5.6, 5.7 | Expected to work, not tested here |

Below 5.8, a few actions and options return "requires UE 5.8" instead of failing; the list is in [PLUGIN-PATCH.md](rust/PLUGIN-PATCH.md). The server reads the editor's engine version and hides what that editor can't run, including the `epic_*` toolset actions, which need 5.8. After merging upstream changes, `rust\tools\check-engine-compat.ps1` recompiles the plugin against every installed engine.

## Troubleshooting

- **The editor isn't found or connected.** `project(action="get_status")` lists the ports tried. The editor publishes its port in `<Project>/Saved/UE_MCP_Bridge/port.json`; `--bridge-port` overrides it.
- **Antivirus and HTTP mode.** Some antivirus web shields, Avast among them, proxy localhost HTTP and corrupt responses, even with the project folder excluded. Use stdio, or exclude `127.0.0.1` in the web shield.
- **Plugin build errors after updating.** Run `rust\tools\check-engine-compat.ps1` and see [PLUGIN-PATCH.md](rust/PLUGIN-PATCH.md).
- **`stop_editor` seems slow.** The editor is showing its own "Save Content" prompt. It never discards work; answer the prompt or save first.

## Repository layout

| Path | What |
|---|---|
| `rust/` | The Rust MCP server, its tests, install scripts and tools |
| `plugin/ue_mcp_bridge/` | The editor plugin (upstream plus this fork's changes) |
| `skills/` | Agent skills for this server, installed into the game by `install-plugin.ps1` |
| `src/`, `docs/` | Upstream's Node server and docs, unchanged; [upstream README](docs/upstream-README.md) |
| `tests/ue_mcp/` | Upstream's UE 5.8 test project |

## Development

```bash
cd rust
cargo test
```

- `cargo test` runs unit and end-to-end tests against a simulated editor.
- `python rust/tools/multi_agent_smoke.py --exe ... --project ... --start-editor` runs three agents against a real editor.
- `rust\tools\check-engine-compat.ps1` compiles the plugin on every installed UE 5.5+.
- `python rust/tools/check_skill_actions.py <ue-mcp-rs.exe> skills` fails if a skill cites an action the server doesn't have.
- CI builds and tests Windows and Linux on every push, and runs the skill check. A `rust-v*` tag publishes a release.

## License

MIT, as upstream ([LICENSE](LICENSE)). Original work © David Bingham, [db-lyon/ue-mcp](https://github.com/db-lyon/ue-mcp).
