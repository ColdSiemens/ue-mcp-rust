---
name: ue-mcp-workflow
description: Use when driving Unreal Engine editor via the ue-mcp MCP server. Covers the required order of operations (status check first), editor lifecycle, working alongside other agents (agent name, locks), engine-version limits, and what to do when the bridge says "still initializing". Pulls in automatically any time the user asks to use an Unreal project or references ue-mcp tools.
---

# ue-mcp workflow

The `ue-mcp` MCP server (`ue-mcp-rs`) exposes one tool per category (action-dispatch style) that drive a live Unreal Engine editor through the `UE_MCP_Bridge` C++ plugin. Every tool takes an `action` and its parameters in `args`: `level(action="get_outliner", args={limit: 5})`. Flat parameters beside `action` are accepted too.

When tool descriptions list only action names (the server's compact mode), get one action's parameters with `project(action="describe_action", args={name: "level.place_actor"})`, and find actions by intent with `project(action="search_tools", args={query: "..."})`.

## Start every session with a status check

**Always** call `project(action="get_status")` before anything else. It tells you:

- whether the editor is connected (`editor.connected`), its engine version and project;
- whether the plugin runs requests concurrently (`concurrentDispatch`);
- which agents are connected and which assets are locked.

If the editor isn't connected, call `editor(action="start_editor")`: it launches the project's editor and returns once the editor reports ready (the first launch can take minutes while shaders compile). Handlers that need the editor world return `"Editor is still initializing. Please wait and retry."` if called too early.

## Working alongside other agents

Several agents can drive one editor at the same time. To keep out of each other's way:

- **Name yourself.** When you are one of several subagents sharing a connection, put `agent: "<your name>"` in the args of every call. Locks belong to that name.
- **Lock before multi-step edits.** `asset(action="lock", args={assetPath})`, do the work, then `asset(action="unlock", args={assetPath})`. A mutation of an asset another agent holds is refused with the holder's name: work on something else, don't retry in a loop.
- **Stay in your area.** Work in the folders, levels or sublevels you were given.
- **Batch.** The editor runs one game-thread operation at a time for everyone; one batch action beats twenty single calls.

## Orient yourself before authoring

Before writing, read. Common orientation calls:

- `level(action="get_outliner")` - what's in the current level
- `asset(action="list", directory="/Game/...", recursive=true)` - what assets exist
- `asset(action="search_fts", query="...")` - ranked search across names/classes/paths (after `asset(action="reindex_fts")` has built the index)
- `reflection(action="reflect_class", className="StaticMeshActor")` - inspect any UE class
- `project(action="list_project_modules")` - native C++ modules in the project

## Mutation recipe

For any write action:

1. Call the read/list variant first to confirm the target exists and capture the current shape (e.g. `blueprint(action="read", assetPath=...)` before `add_node`).
2. Issue the write. Most write handlers return `{ success, existed, created, updated, rollback? }` - honor `existed` as "idempotent no-op" and `updated` as "real change made".
3. If a rollback record came back and you hit a later failure in the same logical unit, call the rollback method yourself.
4. A call that timed out may still have landed. Read the state back before retrying a mutation. Pass `timeoutMs` in args for long operations.

## Engine version limits

The server reads the running editor's engine version. Actions that editor can't run are hidden from the tool list, and a call to one is refused with "not available in the running editor (UE x.y)" or "requires UE 5.8". Nothing was sent to the editor, so **don't retry**: pick another approach (`project(action="search_tools")`) or tell the user. The `epic_*` actions need UE 5.8.

## Editor lifecycle

- `editor(action="stop_editor")` asks the editor to quit. It raises its own "Save Content" prompt for unsaved work and never discards it: save first (`asset(action="save")`), or answer the prompt with `editor(action="list_dialogs")` and `editor(action="respond_to_dialog")`.
- `editor(action="restart_editor")` stops then starts.
- `editor(action="build_project")` compiles C++ with UnrealBuildTool and is refused while the editor runs. For changes to existing classes with the editor open, use `project(action="live_coding_compile")`.

## Common pitfalls

- **Action typos fail** - each tool validates `action`; a typo returns `unknown action '<x>'` with the closest names. Read them rather than guessing.
- **Asset paths use `/Game/...`**, not filesystem paths. Package paths (folder) vs object paths (`Folder/Name.Name`) matter - most create handlers take `packagePath` (folder) plus `name`; most read handlers take `assetPath` (full object path).
- **`execute_python` is an escape hatch, not a shortcut** - if a native action exists for the job, use it.
- **Saving under source control** - on Perforce a file must be checked out before it can be written, and a save to a read-only file is refused. Call `source_control(action="checkout", paths=[...])` first, or `source_control(action="set_checkout_mode", mode="auto")` once to have writes check out for you. `source_control(action="get_provider")` says which provider is active and which actions it supports (`operations`). `history`, `sync`, `mark_for_delete` and `connect` go through the same provider.
- **Editor must be fully loaded** for asset registry queries. If a `list_*` action retries with "still initializing", the editor is still starting - wait a few seconds and retry.

## When something truly can't be done

If no native action covers your case:

1. Confirm no action already covers it: `project(action="search_tools", args={query: ...})` searches the whole surface by intent, and `project(action="describe_action", args={name: "category.action"})` gives one action's parameters.
2. Confirm no UFUNCTION exists to call directly: `reflection(action="list_classes", parentFilter=...)` finds the class and `reflection(action="reflect_class", className=...)` lists what it exposes. A BlueprintCallable function is reachable with `editor(action="invoke_object_function")`.
3. Use `execute_python` as a last resort.
4. Tell the user what was missing, naming the action you would have needed.
