---
name: ue-mcp-native-cpp
description: Use when writing or modifying native C++ UCLASSes in an Unreal project via ue-mcp. Covers the create_cpp_class → edit the files → live_coding_compile or build loop, when to use a full build vs Live Coding, and adding module dependencies. Pulls in any time the user asks to write a new native class, add a UPROPERTY, or implement a UFUNCTION.
---

# ue-mcp native C++ authoring

ue-mcp creates classes and compiles; you read and edit the source files with your own file tools (read, edit, write, search). Nothing in ue-mcp reads or writes `.h`/`.cpp` files for you.

## The authoring loop

1. **Pick a module.** `project(action="list_project_modules")` enumerates the project's native modules (name, host type, source path). Feed the module name into `create_cpp_class`.
2. **Create the class.** `project(action="create_cpp_class", className, parentClass?, moduleName?, classDomain?, subPath?)` writes `.h` and `.cpp` using the same engine template that `File → New C++ Class` uses. The class-name prefix (`A`, `U`, `F`) is derived from `parentClass` - pass the bare name (`"BotSpawner"`, not `"ABotSpawner"`). The result gives the file paths.
3. **Edit the files** under the project's `Source/` folder with your own tools. Read first, then edit.
4. **Compile.**
   - **New UCLASS** → a full build and an editor restart: Live Coding doesn't reliably register brand-new UCLASSes. `create_cpp_class` reports this as `needsEditorRestart`. The build is refused while the editor runs, so: `editor(action="stop_editor")` → `project(action="build")` → `editor(action="start_editor")`.
   - **Existing UCLASS** (method bodies, new UPROPERTYs on an existing class, etc.) → `project(action="live_coding_compile", wait=true)` with the editor open. It hot-patches without a restart - the fast inner loop. Check `project(action="live_coding_status")` first to confirm Live Coding is available and not already compiling.
   - A build reports `errors` (file, line and message) and the last lines of output. Fix those and build again.

## Adding module dependencies

Edit the module's `<Module>.Build.cs` yourself: add the module name to `PublicDependencyModuleNames` (needed by your public headers) or `PrivateDependencyModuleNames` (only by your `.cpp` files). Then do a full build.

## Reading the engine source

When you need to know *how* UE does something:

- **Engine source on disk.** It's under the engine's `Engine/Source` (and `Engine/Plugins`). `editor(action="start_editor")` and `project(action="get_status")` show which engine the project uses. Launcher installs ship the headers, so search and read them with your own tools.
- **What a class exposes to reflection.** `reflection(action="reflect_class", className=...)` lists its properties and functions, as the running editor sees them.

## Pitfalls

- **UCLASSes need UnrealBuildTool + MSVC/Clang** on the host. Same requirement as the editor's own Build menu.
- **Keep edits inside `Source/`.** Never edit `Intermediate/` or `Binaries/`; they are regenerated.
- **Live Coding ≠ full build.** If behavior doesn't reflect your code changes, check whether you added a new UCLASS, UFUNCTION reflection macro, or UPROPERTY annotation - those usually need a full build.
- **Other agents share the editor.** Stopping it for a build interrupts everyone: say so before you do it.

## Testing the new class

Once a class is compiled and registered, you can typically:

- `reflection(action="reflect_class", className=...)` to verify UE sees the UCLASS.
- `blueprint(action="create", parentClass="/Script/<Module>.<Class>")` to subclass it from a Blueprint.
- `level(action="place_actor", actorClass=...)` to drop an instance into the level.
