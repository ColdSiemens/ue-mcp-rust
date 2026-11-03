<#
.SYNOPSIS
    Copies this fork's concurrency change into an installed UE_MCP_Bridge plugin.

.DESCRIPTION
    Replaces BridgeServer.cpp/.h and adds BridgeThrottle.cpp/.h under
    <PluginRoot>\Source\UE_MCP_Bridge\Private, taking them from this
    repository's plugin\ue_mcp_bridge folder. Every file it replaces is backed
    up to <file>.orig first (an existing .orig is never overwritten, so a second
    run keeps the true original). Then the plugin has to be rebuilt.

    The change was made against upstream ue-mcp commit cfa15afb (plugin 1.4.12-beta.1).
    If the installed BridgeServer files are a different version the script stops,
    because whole-file copies over other versions may not compile. Use -Force to
    copy anyway, or cherry-pick this fork's feat(bridge) commit onto that version.

.PARAMETER PluginRoot
    The plugin folder holding UE_MCP_Bridge.uplugin, e.g. C:\MyGame\Plugins\UE_MCP_Bridge.

.PARAMETER Force
    Copy even when the installed files are not the version the patch was made against.

.PARAMETER SetAsideBinaries
    Rename the plugin's Binaries and Intermediate folders (to *.pre-patch-<timestamp>)
    so the editor finds no prebuilt module and offers to rebuild it on next launch.
    Use this for a Blueprint-only project or a plugin installed with prebuilt binaries.
    Nothing is deleted.

.EXAMPLE
    .\rust\apply-plugin-patch.ps1 -PluginRoot C:\MyGame\Plugins\UE_MCP_Bridge -SetAsideBinaries
#>
param(
    [Parameter(Mandatory = $true)][string]$PluginRoot,
    [switch]$Force,
    [switch]$SetAsideBinaries
)

$ErrorActionPreference = 'Stop'

$BaseCommit = 'cfa15afb810e3be1c3f9b69048614ede6dad55fd'
# SHA-256 of the upstream files at $BaseCommit, UTF-8, line endings normalized to LF.
$BaseHashes = @{
    'BridgeServer.cpp' = '3cc0efe1ac7afcabbd2bc8a3d22e2c23d7021a5ff9e470da855d95f34e7848fe'
    'BridgeServer.h'   = '3acc7033b11bc4bc80677c41207c2315ca819d040fbe631afc8551c709bf4dc9'
}
$PatchedFiles = @('BridgeServer.cpp', 'BridgeServer.h', 'BridgeThrottle.cpp', 'BridgeThrottle.h')

function Get-NormalizedHash([string]$Path) {
    $text = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($Path))
    if ($text.Length -gt 0 -and $text[0] -eq [char]0xFEFF) { $text = $text.Substring(1) }
    $text = $text -replace "`r`n", "`n"
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { $hash = $sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($text)) }
    finally { $sha.Dispose() }
    return ([System.BitConverter]::ToString($hash) -replace '-', '').ToLowerInvariant()
}

$PluginRoot = (Resolve-Path -LiteralPath $PluginRoot).Path
if (-not (Test-Path -LiteralPath (Join-Path $PluginRoot 'UE_MCP_Bridge.uplugin'))) {
    throw "No UE_MCP_Bridge.uplugin in '$PluginRoot'. Pass the plugin folder, e.g. <Project>\Plugins\UE_MCP_Bridge."
}
$TargetDir = Join-Path $PluginRoot 'Source\UE_MCP_Bridge\Private'
if (-not (Test-Path -LiteralPath $TargetDir)) {
    throw "'$TargetDir' does not exist. This install has no plugin source, and the patch has to be compiled. Install the plugin with its Source folder first."
}
$SourceDir = Join-Path $PSScriptRoot '..\plugin\ue_mcp_bridge\Source\UE_MCP_Bridge\Private'

# Already applied? Every patched file in place and identical.
$alreadyApplied = $true
foreach ($name in $PatchedFiles) {
    $target = Join-Path $TargetDir $name
    if (-not (Test-Path -LiteralPath $target) -or
        (Get-NormalizedHash $target) -ne (Get-NormalizedHash (Join-Path $SourceDir $name))) {
        $alreadyApplied = $false
        break
    }
}
if ($alreadyApplied) {
    Write-Host "The patch is already applied in $TargetDir. Nothing to do."
    exit 0
}

# Version check: only the two files the patch replaces matter.
$mismatched = @()
foreach ($name in $BaseHashes.Keys) {
    $target = Join-Path $TargetDir $name
    if (-not (Test-Path -LiteralPath $target)) {
        $mismatched += "$name (missing)"
    }
    elseif ((Get-NormalizedHash $target) -ne $BaseHashes[$name]) {
        $mismatched += $name
    }
}
if ($mismatched.Count -gt 0 -and -not $Force) {
    throw ("The installed plugin is not the version this patch was made against (upstream $BaseCommit). " +
        "Different: $($mismatched -join ', '). Copying whole files over another version may not compile. " +
        "Options: update the plugin to that upstream version first; or cherry-pick this fork's " +
        "feat(bridge) commit onto your version; or rerun with -Force.")
}
if ($mismatched.Count -gt 0) {
    Write-Warning "Installed files differ from the patch base ($($mismatched -join ', ')); copying anyway because of -Force."
}

foreach ($name in $PatchedFiles) {
    $target = Join-Path $TargetDir $name
    if (Test-Path -LiteralPath $target) {
        $backup = "$target.orig"
        if (-not (Test-Path -LiteralPath $backup)) {
            Copy-Item -LiteralPath $target -Destination $backup
            Write-Host "Backed up $name -> $name.orig"
        }
    }
    Copy-Item -LiteralPath (Join-Path $SourceDir $name) -Destination $target -Force
    Write-Host "Installed $name"
}

# UBT caches this module's file list and rescans only when Build.cs changes
# (see the comment at the top of UE_MCP_Bridge.Build.cs). BridgeThrottle.cpp is new.
$buildCs = Join-Path $PluginRoot 'Source\UE_MCP_Bridge\UE_MCP_Bridge.Build.cs'
if (Test-Path -LiteralPath $buildCs) {
    (Get-Item -LiteralPath $buildCs).LastWriteTime = Get-Date
    Write-Host "Touched UE_MCP_Bridge.Build.cs so UnrealBuildTool picks up the new source file"
}

if ($SetAsideBinaries) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    foreach ($folder in @('Binaries', 'Intermediate')) {
        $path = Join-Path $PluginRoot $folder
        if (Test-Path -LiteralPath $path) {
            Rename-Item -LiteralPath $path -NewName "$folder.pre-patch-$stamp"
            Write-Host "Renamed $folder -> $folder.pre-patch-$stamp"
        }
    }
}

Write-Host ""
Write-Host "Done. Rebuild the plugin before the change takes effect:"
Write-Host "  - C++ project: close the editor, then build the project's Editor target (Development Editor) in your IDE."
Write-Host "  - Blueprint-only project: rerun with -SetAsideBinaries if you have not, then open the project and accept the prompt to rebuild UE_MCP_Bridge."
Write-Host "Check: get_bridge_capabilities lists 'concurrent-dispatch' under features. If 'Use Less CPU when in"
Write-Host "  Background' is on, the log also says 'background CPU throttling suspended' once a client connects."
Write-Host "To undo: copy each *.orig back over its file, delete BridgeThrottle.cpp/.h, and rebuild."
