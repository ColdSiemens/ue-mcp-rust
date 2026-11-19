# ue-mcp server for this project

This folder holds the MCP server (`ue-mcp-rs.exe`) that matches the plugin around it, and `ue-mcp.cmd`, which starts it. The project's `.mcp.json` runs the launcher, so the server comes with a version-control sync like the rest of the project:

```json
{
  "mcpServers": {
    "ue-mcp": {
      "command": "cmd",
      "args": ["/d", "/c", "Plugins\\UE_MCP_Bridge\\Server\\ue-mcp.cmd", "--compact"]
    }
  }
}
```

There is nothing to install per machine. Claude Code asks once to approve the project's `ue-mcp` server, and again whenever its command in `.mcp.json` changes. On first start the launcher copies the exe to `%LOCALAPPDATA%\ue-mcp-rs\<hash>\` and runs that copy, because a running exe is locked and a sync could not replace it here. A new build gets a new folder; older folders are removed once no session runs them.

## For AI agents: the ue-mcp tools are missing or fail to connect

Work through these in order. Don't download a server from elsewhere, and don't edit `.mcp.json` to point at another copy: the server must be the one in this folder, because the plugin refuses a server older than itself.

1. **Was the session started at the project root?** The paths in `.mcp.json` are relative to the folder with the `.uproject`. Start Claude Code there.
2. **Is the server here?** `ue-mcp-rs.exe` and `ue-mcp.cmd` must exist in this folder. If they don't, sync this folder from version control.
3. **Does it start?** From the project root, run:
   ```
   cmd /d /c Plugins\UE_MCP_Bridge\Server\ue-mcp.cmd --version
   ```
   It prints `ue-mcp-rust <version>`. An error names what is wrong, such as a copy into `%LOCALAPPDATA%` that failed.
4. **Tools listed, but every call says the editor is not connected?**
   - Open the project in the editor. `project(action="get_status")` shows the ports tried.
   - "The editor refused the session token": the server runs as a different Windows user than the editor, or `UE_MCP_BRIDGE_TOKEN_DIR` differs between them.
   - The plugin must be built. With a source engine, build the Editor target after syncing.
5. **After a sync that changed the plugin**, restart the editor (rebuilt plugin) and the Claude Code sessions (new server). A session started before the sync keeps the server it started with.

Tell the user what you found. Don't change the plugin, the launcher or `.mcp.json` to work around a problem.

## Updating

The plugin and the server must change together. Install both with `install-plugin.ps1` from the ue-mcp-rust repository, build the editor target, check it, then submit `Plugins/UE_MCP_Bridge` (including this folder) and `.mcp.json` in one change.
