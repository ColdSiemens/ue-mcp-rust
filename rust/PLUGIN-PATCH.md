# UE_MCP_Bridge changes in this fork

This fork's `plugin/ue_mcp_bridge` is upstream commit `cfa15afb810e3be1c3f9b69048614ede6dad55fd` (plugin 1.4.12-beta.1) plus four changes:
1. Concurrent requests per socket.
2. No background throttling while a client is connected.
3. Builds on UE 5.5.4.
4. A batch node-move handler for Blueprint layout.

Status:
- Compiled on UE 5.8.2 and UE 5.5.4. `rust/tools/check-engine-compat.ps1` repeats the check.
- Live-tested on UE 5.8.2 and 5.5.4: start, reads, one-socket concurrency, parallel reads, stop, and Blueprint layout with undo all passed. The editor log shows throttling suspended.

## What changes

**1. One socket runs many requests at once** (`BridgeServer.cpp` / `.h`)

- Before: the connection's reader called `ProcessMessage` inline and read nothing more until the game thread answered. A client that sends several agents' calls down one socket queued them all behind the slowest one.
- After: each complete message gets its own worker thread (`Async(EAsyncExecution::Thread)`, not the thread pool, because workers block for the whole game-thread wait). The worker runs `ProcessMessage` and writes its own answer.
- Every write to the socket goes through one per-connection send lock (`FMCPConnectionChannel`), so frames never interleave. Nothing is sent after a close frame.
- At most 32 requests can be in flight per connection. At the cap the reader stops reading, so TCP pushes back on the client.
- Before the reader returns and the socket closes, it waits for every worker, on every exit path. On shutdown the executor's existing `BeginShutdown` makes those waits end within one 50 ms slice.
- `get_bridge_capabilities` adds `"concurrent-dispatch"` to `features` while this mode is on.
- Unchanged: handlers still run one at a time on the game thread. Concurrent requests just queue for the same tick instead of queueing behind each other first. `ProcessMessage` was already re-entrant, because separate connections always ran it in parallel.
- New benefit: while one request waits on a modal dialog, `respond_to_dialog` can arrive on the same socket.

**2. No background throttling while a client is connected** (new `BridgeThrottle.cpp` / `.h`)

- When the first client finishes the WebSocket upgrade, `UEditorPerformanceSettings::bThrottleCPUWhenNotForeground` is set to false on the game thread. When the last client leaves, the user's own value is put back.
- The change is in memory only: no `SaveConfig` and no `PostEditChange`.
- It waits for `GEditor` before touching the setting, because the module loads at PostConfigInit and a retrying client can connect earlier than that.
- Module shutdown removes any queued update and restores the setting.
- Interplay with the automation-test handler (`EditorHandlers.cpp` ~4866): that handler only restores what it changed itself. While the bridge is holding the flag off it finds it already off and does nothing.

**3. Builds on UE 5.5.4** (handler and header gates)

Upstream says it supports 5.4–5.8, but 153 distinct compile errors and 5 link errors stopped it building on 5.5.4. Most came from gates that assumed an API arrived in 5.5 when it actually arrived later. Every fix was checked against the installed 5.5.4 and 5.8.2 headers:

- Where upstream already had an older-engine path, the gate moved to the version the API was actually found in. Usually that is 5.8, because 5.6 and 5.7 couldn't be checked, and a path that exists in both 5.5 and 5.8 is safe in between.
- Where 5.5 has an equivalent, a 5.5 branch uses it. Examples: MetaSound builder lookups through the frontend document builder; landscape edit layers through `GetLayers()`; StateTree `SetLinkedState` done step by step.
- Classes that 5.5 declares without an export macro are resolved by reflection path, e.g. `/Script/Engine.WorldPartitionRuntimeCellTransformer`.
- The 5.8 code path is upstream's code, unchanged.

What is missing below UE 5.8 (verified absent in 5.5.4; may exist in 5.6/5.7):

| Area | Behaviour on 5.5 |
|---|---|
| `list_niagara_module_inputs`, `set_niagara_module_input`, `set_niagara_dynamic_input` | Return "requires UE 5.8": the NiagaraEditor stack API isn't exported before 5.8. |
| `metasound_remove_member` with `memberKind: "variable"` | Returns "requires UE 5.8". Inputs and outputs still work. |
| `set_pose_search_database_settings` with `poseSearchMode: "eventonly"` | Returns "requires UE 5.8" (the mode doesn't exist). |
| `request_state_tree_transition` with `fallback` | Returns "requires UE 5.8". The transition itself works. |
| `epic_*` actions | Need UE 5.8's toolset registry; the Rust server hides them on older editors. |
| Output fields | Omitted where 5.5 has no data: StateTree schema flags and `tasksCompletion`, `description` (needs 5.6), `numExternalObjects`, plugin `mounted`, trace bytes emitted. Relay trace states read "unknown"; sound concurrency reports the authored `MaxCount`. |
| Widget variable GUIDs | Not synced, because 5.5 widget blueprints have no GUID map. |

**4. `set_node_positions`** (`BlueprintHandlers_Graph.cpp`)

This moves many nodes of one graph in a single game-thread call: `{assetPath, graphName?, positions: [{nodeId, posX, posY}]}`.
- **Before:** layout tools could only call `set_node_position` once per node. Each call cost a round trip, and an open graph view redrew every time (~0.45 s per node, 134 s for 300 nodes). Nothing could be undone.
- **Undo:** all moves run in one `FScopedTransaction`, with `Modify()` before each write, so a single Ctrl+Z restores the graph.
- **Refresh:** the graph is notified once.
- **Result:** it reports `moved`, `unchanged`, and the node IDs it couldn't find.
- **Who uses it:** the Rust server's `blueprint(auto_layout)` calls it when the plugin registers it.

## Opt-outs

| Setting | Effect |
|---|---|
| `UE_MCP_SERIAL_DISPATCH=1` or `-MCPSerialDispatch` | Old behaviour: one request at a time per connection. `concurrent-dispatch` is not advertised. |
| `UE_MCP_KEEP_THROTTLE=1` or `-MCPKeepThrottle` | Leave the throttle setting alone. |

## Install

From the repository root:

```powershell
.\rust\install-plugin.ps1 -Project C:\MyGame\MyGame.uproject
```

- Copies the whole plugin source into `<Project>\Plugins\UE_MCP_Bridge`. An existing copy (stock, or patched by hand) is moved to `<Project>\Saved\UE_MCP_Bridge-backup-<timestamp>`, since two copies under `Plugins\` would clash.
- No binaries are copied. The module is compiled for the project's own engine on the next build: a C++ project builds its Editor target; a Blueprint-only project accepts the editor's rebuild prompt, which needs Visual Studio's C++ tools.
- The upstream ue-mcp updater (`ue-mcp-update`, or the Node server's bridge auto-update) redeploys stock plugin sources and will overwrite this one. Turn it off, or rerun the script afterwards.
- After merging a newer upstream into this fork, run `rust\tools\check-engine-compat.ps1` before installing.

## Verify

- `get_bridge_capabilities` → `features` contains `"concurrent-dispatch"`.
- If "Use Less CPU when in Background" is on, the Output Log shows `[UE-MCP] Bridge client connected; background CPU throttling suspended ...` after a client connects, and `... restored.` after the last one disconnects.
- Timing check: on one socket, send a slow call (an `execute_python` that sleeps 3 s), then `get_engine_state` straight after it. Before the patch, `get_engine_state` answers after about 3 s, because it waits behind the slow call on the socket. After the patch it answers at once, because it is served off the game thread. Game-thread calls still wait for one another.

## Known risks

- **Answer order.** Requests on one socket may now finish in a different order than they arrived, and two requests sent back-to-back may start in either order. Every known client matches answers by `id` and awaits each call before sending the next. A client that pipelines dependent calls without waiting needs `UE_MCP_SERIAL_DISPATCH=1`.
- **Unload window.** A worker's last action is dropping the in-flight count. After that, its thread still runs a few instructions of `Async` plumbing from this module. Upstream's connection threads already have the same window. It only matters if the DLL is unloaded mid-session (Live Coding of this plugin, or disabling it), not at editor exit.
- **Throttle setting.** If the user changes any "Editor Performance" preference while a client is connected, the settings UI saves that whole section, including the temporary `false`. Untick "Use Less CPU when in Background" again afterwards if that matters.
- **Throttle without `PostEditChange`.** Assumed to work because the editor reads the flag every frame. Upstream's own toggle calls `PostEditChange()`. If throttling still happens with a client connected, add `Settings->PostEditChange();` after each write in `BridgeThrottle.cpp`.
- **Idle hub.** A client that stays connected while idle (the Rust hub keeps a pool of sockets open) keeps the editor at full frame rate in the background, so it uses more CPU and GPU while idle.
