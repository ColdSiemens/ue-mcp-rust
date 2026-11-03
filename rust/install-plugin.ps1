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

.PARAMETER Project
    The project's .uproject file, or the folder holding it.

.EXAMPLE
    .\rust\install-plugin.ps1 -Project D:\Games\MyGame\MyGame.uproject
#>
param(
    [Parameter(Mandatory = $true)][string]$Project
)

$ErrorActionPreference = 'Stop'

$source = (Resolve-Path (Join-Path $PSScriptRoot '..\plugin\ue_mcp_bridge')).Path
$Project = (Resolve-Path $Project).Path
$projectDir = if ((Get-Item $Project).PSIsContainer) { $Project } else { Split-Path $Project -Parent }
$uproject = Get-ChildItem $projectDir -Filter *.uproject | Select-Object -First 1
if (-not $uproject) { throw "No .uproject in '$projectDir'." }

$target = Join-Path $projectDir 'Plugins\UE_MCP_Bridge'
if (Test-Path $target) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $backup = Join-Path $projectDir "Saved\UE_MCP_Bridge-backup-$stamp"
    New-Item -ItemType Directory -Force (Split-Path $backup -Parent) | Out-Null
    Move-Item $target $backup
    Write-Host "Moved the existing plugin to $backup"
}

robocopy $source $target /E /XD Binaries Intermediate /NFL /NDL /NJH /NJS /NP | Out-Null
if ($LASTEXITCODE -ge 8) { throw "Copying the plugin failed (robocopy exit $LASTEXITCODE)." }
Write-Host "Installed UE_MCP_Bridge into $target"

$association = (Get-Content $uproject.FullName -Raw | ConvertFrom-Json).EngineAssociation
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
Write-Host "Undo: delete $target and move the backup from Saved\ back to Plugins\UE_MCP_Bridge."
# robocopy reports success as 1-7; don't let that read as a failure to callers.
exit 0
