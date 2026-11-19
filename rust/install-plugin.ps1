<#
.SYNOPSIS
    Installs this fork's UE_MCP_Bridge plugin into an Unreal project.

.DESCRIPTION
    Copies plugin\ue_mcp_bridge from this repository to <Project>\Plugins\UE_MCP_Bridge.
    An existing copy there (stock, or patched by hand) is moved to
    <Project>\Saved\UE_MCP_Bridge-backup-<timestamp> first; it can't stay under
    Plugins\, because two plugins with one name stop the editor from loading.
    Binaries and Intermediate are not copied, so the module is compiled for
    the project's own engine (UE 5.5 through 5.8) on the next build.

    Puts the server, ue-mcp-rs.exe, in Plugins\UE_MCP_Bridge\Server\ with the
    ue-mcp.cmd launcher, and points the project's .mcp.json at the launcher.
    The server then reaches a team through version control together with the
    plugin it must match, and nobody installs anything per machine. The
    launcher runs a copy under %LOCALAPPDATA%\ue-mcp-rs, so a sync can always
    replace the exe. Other servers in .mcp.json are kept; the old file goes to
    Saved\mcp.json-backup-<timestamp>. Server\README.md tells agents what to do
    when the tools don't connect.

    Also copies this repository's ue-mcp-* agent skills to
    <Project>\.claude\skills\, where Claude Code finds them. Only those
    folders are replaced (the old ones go to Saved\ue-mcp-skills-backup-<timestamp>);
    other skills in the project are left alone.

.PARAMETER Project
    The project's .uproject file, or the folder holding it.

.PARAMETER ServerExe
    The ue-mcp-rs.exe to ship. Default: this repository's rust\target\release
    build. Without Rust, take it from a release zip.

.PARAMETER SkipServer
    Leave Server\ and .mcp.json alone (the server is installed some other way).

.PARAMETER SkipSkills
    Install the plugin only, without the agent skills.

.EXAMPLE
    .\rust\install-plugin.ps1 -Project D:\Games\MyGame\MyGame.uproject
#>
param(
    [Parameter(Mandatory = $true)][string]$Project,
    [string]$ServerExe,
    [switch]$SkipServer,
    [switch]$SkipSkills
)

$ErrorActionPreference = 'Stop'

$source = (Resolve-Path (Join-Path $PSScriptRoot '..\plugin\ue_mcp_bridge')).Path
$Project = (Resolve-Path $Project).Path
$projectDir = if ((Get-Item $Project).PSIsContainer) { $Project } else { Split-Path $Project -Parent }
$uproject = Get-ChildItem $projectDir -Filter *.uproject | Select-Object -First 1
if (-not $uproject) { throw "No .uproject in '$projectDir'." }
# Windows PowerShell 5.1 has no $PSScriptRoot in parameter defaults.
if (-not $ServerExe) { $ServerExe = Join-Path $PSScriptRoot 'target\release\ue-mcp-rs.exe' }
if (-not $SkipServer -and -not (Test-Path $ServerExe)) {
    throw "No server at '$ServerExe'. Build it (cargo build --release in rust\), or pass -ServerExe with the ue-mcp-rs.exe from a release zip."
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$target = Join-Path $projectDir 'Plugins\UE_MCP_Bridge'
if (Test-Path $target) {
    $backup = Join-Path $projectDir "Saved\UE_MCP_Bridge-backup-$stamp"
    New-Item -ItemType Directory -Force (Split-Path $backup -Parent) | Out-Null
    Move-Item $target $backup
    Write-Host "Moved the existing plugin to $backup"
}

robocopy $source $target /E /XD Binaries Intermediate /NFL /NDL /NJH /NJS /NP | Out-Null
if ($LASTEXITCODE -ge 8) { throw "Copying the plugin failed (robocopy exit $LASTEXITCODE)." }
Write-Host "Installed UE_MCP_Bridge into $target"

if (-not $SkipServer) {
    $serverDir = Join-Path $target 'Server'
    New-Item -ItemType Directory -Force $serverDir | Out-Null
    Copy-Item $ServerExe (Join-Path $serverDir 'ue-mcp-rs.exe') -Force
    Copy-Item (Join-Path $PSScriptRoot 'launcher\ue-mcp.cmd'), (Join-Path $PSScriptRoot 'launcher\README.md') $serverDir -Force
    $version = & $ServerExe --version
    Write-Host "Installed the server ($version) and its launcher into $serverDir"

    # The launcher finds the project from its own location; the command is
    # relative to the project root, where Claude Code starts the server.
    $mcpJson = Join-Path $projectDir '.mcp.json'
    $flags = @('--compact')
    $config = [pscustomobject]@{ mcpServers = [pscustomobject]@{} }
    if (Test-Path $mcpJson) {
        $config = Get-Content $mcpJson -Raw | ConvertFrom-Json
        if (-not $config.mcpServers) { $config | Add-Member -Force mcpServers ([pscustomobject]@{}) }
        $old = $config.mcpServers.'ue-mcp'
        if ($old) {
            $kept = @($old.args | Where-Object { $_ -like '--*' })
            if ($kept.Count) { $flags = $kept }
        }
        $mcpBackup = Join-Path $projectDir "Saved\mcp.json-backup-$stamp"
        New-Item -ItemType Directory -Force (Split-Path $mcpBackup -Parent) | Out-Null
        Copy-Item $mcpJson $mcpBackup
    }
    $entry = [pscustomobject]@{ command = 'cmd'; args = @('/d', '/c', 'Plugins\UE_MCP_Bridge\Server\ue-mcp.cmd') + $flags }
    $config.mcpServers | Add-Member -Force 'ue-mcp' $entry
    [IO.File]::WriteAllText($mcpJson, ($config | ConvertTo-Json -Depth 20) + "`r`n")
    Write-Host "Pointed .mcp.json's ue-mcp server at the launcher ($($flags -join ' '))"
}

if (-not $SkipSkills) {
    $skillsTarget = Join-Path $projectDir '.claude\skills'
    $skillsBackup = Join-Path $projectDir "Saved\ue-mcp-skills-backup-$stamp"
    $skills = @(Get-ChildItem (Join-Path $PSScriptRoot '..\skills') -Directory -Filter 'ue-mcp-*')
    foreach ($skill in $skills) {
        $dest = Join-Path $skillsTarget $skill.Name
        if (Test-Path $dest) {
            New-Item -ItemType Directory -Force $skillsBackup | Out-Null
            Move-Item $dest (Join-Path $skillsBackup $skill.Name)
        }
        robocopy $skill.FullName $dest /E /NFL /NDL /NJH /NJS /NP | Out-Null
        if ($LASTEXITCODE -ge 8) { throw "Copying skill $($skill.Name) failed (robocopy exit $LASTEXITCODE)." }
    }
    Write-Host "Installed $($skills.Count) agent skills into $skillsTarget"
    if (Test-Path $skillsBackup) { Write-Host "Moved the previous ue-mcp-* skills to $skillsBackup" }
}

$association = (Get-Content $uproject.FullName -Raw | ConvertFrom-Json).EngineAssociation
# A project inside a source-built engine's tree has no association.
if (-not $association) { $association = 'the source build it sits in' }
Write-Host ""
Write-Host "Next: compile it for this project's engine ($association)."
if (Test-Path (Join-Path $projectDir 'Source')) {
    Write-Host "  C++ project: close the editor and build the Editor target in your IDE,"
    Write-Host "  or let ue-mcp-rs do it: editor(action=`"build_project`")."
}
else {
    Write-Host "  Blueprint-only project: open it and accept the prompt to rebuild UE_MCP_Bridge"
    Write-Host "  (needs Visual Studio with the C++ workload)."
}
Write-Host "Check: project(action=`"get_status`") shows concurrentDispatch true and your engine version."
if (-not $SkipServer) {
    Write-Host "Team: submit Plugins\UE_MCP_Bridge (with Server\) and .mcp.json in one change; the plugin refuses an older server."
}
Write-Host "Undo: delete $target and move the backup from Saved\ back to Plugins\UE_MCP_Bridge."
# robocopy reports success as 1-7; don't let that read as a failure to callers.
exit 0
