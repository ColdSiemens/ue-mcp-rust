<#
.SYNOPSIS
    Installs the prebuilt ue-mcp-rs server from the fork's GitHub Releases. No Rust needed.

.DESCRIPTION
    Downloads ue-mcp-rs-windows-x64.zip from the newest release, unpacks it to
    -InstallDir, and, given -Project, registers it with Claude Code as an MCP
    server for that project. Run it again to update.

.PARAMETER Project
    The game's .uproject. When given, the server is registered with
    `claude mcp add` (or the command to do so is printed if Claude Code is not on PATH).

.PARAMETER Name
    The MCP server name agents see the tools under. Default: ue.

.EXAMPLE
    .\install.ps1 -Project D:\Games\MyGame\MyGame.uproject
#>
param(
    [string]$Project,
    [string]$Name = 'ue',
    [string]$Repo = 'ColdSiemens/ue-mcp-rust',
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'ue-mcp-rs')
)

$ErrorActionPreference = 'Stop'
$assetName = 'ue-mcp-rs-windows-x64.zip'

$releases = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases"
$release = $releases | Where-Object { $_.tag_name -like 'rust-v*' -and -not $_.draft } | Select-Object -First 1
if (-not $release) { throw "No rust-v* release found in $Repo." }
$asset = $release.assets | Where-Object { $_.name -eq $assetName }
if (-not $asset) { throw "Release $($release.tag_name) has no $assetName." }

$zip = Join-Path $env:TEMP $assetName
Invoke-WebRequest $asset.browser_download_url -OutFile $zip
New-Item -ItemType Directory -Force $InstallDir | Out-Null
Expand-Archive $zip -DestinationPath $InstallDir -Force
Remove-Item $zip
$exe = Join-Path $InstallDir 'ue-mcp-rs.exe'
Unblock-File $exe
Write-Host "Installed ue-mcp-rs $($release.tag_name) to $exe"

if ($Project) {
    $Project = (Resolve-Path $Project).Path
    if (Get-Command claude -ErrorAction SilentlyContinue) {
        & claude mcp add $Name -- $exe $Project --compact
    }
    else {
        Write-Host "Claude Code is not on PATH. Register the server with:"
        Write-Host "  claude mcp add $Name -- `"$exe`" `"$Project`" --compact"
    }
}
