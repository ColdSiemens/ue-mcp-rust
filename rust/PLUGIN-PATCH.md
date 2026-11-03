# UE_MCP_Bridge concurrency change

> **Not compiled yet.** It was written without an Unreal toolchain and has only been reviewed by reading. Build it before relying on it. If the build fails, the opt-outs below won't help; restore the `.orig` files instead.

This fork's `plugin/ue_mcp_bridge` carries the change as one commit (`feat(bridge): run requests concurrently per socket, stop background throttling`) on top of upstream commit `cfa15afb810e3be1c3f9b69048614ede6dad55fd` (plugin 1.4.12-beta.1). `git show` on that commit is the patch.

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

## Opt-outs

| Setting | Effect |
|---|---|
| `UE_MCP_SERIAL_DISPATCH=1` or `-MCPSerialDispatch` | Old behaviour: one request at a time per connection. `concurrent-dispatch` is not advertised. |
| `UE_MCP_KEEP_THROTTLE=1` or `-MCPKeepThrottle` | Leave the throttle setting alone. |

## Install

To patch a project that already has the upstream plugin installed, run from the repository root:

```powershell
.\rust\apply-plugin-patch.ps1 -PluginRoot C:\MyGame\Plugins\UE_MCP_Bridge                    # C++ project: then build the Editor target
.\rust\apply-plugin-patch.ps1 -PluginRoot C:\MyGame\Plugins\UE_MCP_Bridge -SetAsideBinaries  # Blueprint-only / prebuilt binaries
```

- The script copies the four changed files from this repository's `plugin/` folder. It refuses if the installed `BridgeServer.cpp`/`.h` are not the 1.4.12-beta.1 version. If yours differ, update the plugin to that version first, or cherry-pick the commit onto your version.
- Originals are kept as `*.orig`. `-SetAsideBinaries` renames `Binaries`/`Intermediate` (it deletes nothing) so the editor offers to rebuild the module.
- The upstream ue-mcp updater (`ue-mcp-update`, or the Node server's bridge auto-update) redeploys stock plugin sources and will overwrite this change. Turn it off or re-apply afterwards.
- After merging a newer upstream into this fork, the base hashes in `apply-plugin-patch.ps1` need updating to the new upstream version.

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
