<#
.SYNOPSIS
    Compiles the UE_MCP_Bridge plugin against each installed Unreal Engine, so a
    call into an API one of them lacks is caught before the plugin is deployed.

.DESCRIPTION
    For every engine version (default: each launcher install from 5.5 up), makes
    a throwaway C++ project under -WorkDir, copies the plugin into it, and builds
    the editor target. Prints one line per engine and the first errors of any
    that fail. Exit code 1 when any engine fails.

    Run it after merging upstream into this fork. A fix belongs behind a version
    check (see Public/MCPEngineCompat.h), never in place of the newer code path.

.PARAMETER Versions
    Engine versions to check, e.g. 5.5,5.8. Default: every installed 5.5+.

.PARAMETER EngineRoot
    Check one engine by folder instead (e.g. a source build), labelled -Label.

.EXAMPLE
    .\rust\tools\check-engine-compat.ps1
    .\rust\tools\check-engine-compat.ps1 -Versions 5.5
    .\rust\tools\check-engine-compat.ps1 -EngineRoot D:\UE_5.5.4 -Label source-5.5.4
#>
param(
    [string[]]$Versions,
    [string]$EngineRoot,
    [string]$Label = 'custom',
    [string]$PluginDir = (Join-Path $PSScriptRoot '..\..\plugin\ue_mcp_bridge'),
    # Short on purpose: UE build paths fail past the 260-character Windows limit.
    [string]$WorkDir = (Join-Path $env:TEMP 'uemcp-compat')
)

$ErrorActionPreference = 'Stop'
$PluginDir = (Resolve-Path $PluginDir).Path

function Get-InstalledEngines {
    $key = 'HKLM:\SOFTWARE\EpicGames\Unreal Engine'
    if (-not (Test-Path $key)) { return @() }
    Get-ChildItem $key | ForEach-Object {
        $dir = (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).InstalledDirectory
        [version]$v = $null
        if ($dir -and [version]::TryParse($_.PSChildName, [ref]$v) -and $v -ge [version]'5.5' -and (Test-Path $dir)) {
            [pscustomobject]@{ Label = $_.PSChildName; Version = $_.PSChildName; Root = $dir }
        }
    } | Sort-Object { [version]$_.Version }
}

if ($EngineRoot) {
    $build = Get-Content (Join-Path $EngineRoot 'Engine\Build\Build.version') -Raw | ConvertFrom-Json
    $engines = @([pscustomobject]@{ Label = $Label; Version = "$($build.MajorVersion).$($build.MinorVersion)"; Root = $EngineRoot })
}
else {
    $engines = @(Get-InstalledEngines)
    if ($Versions) { $engines = @($engines | Where-Object { $Versions -contains $_.Version }) }
}
if ($engines.Count -eq 0) { throw 'No Unreal Engine 5.5+ found. Pass -Versions or -EngineRoot.' }

function New-CompatProject([string]$Dir, [string]$Name, [string]$Association) {
    New-Item -ItemType Directory -Force (Join-Path $Dir "Source\$Name") | Out-Null
    @"
{
	"FileVersion": 3,
	"EngineAssociation": "$Association",
	"Modules": [ { "Name": "$Name", "Type": "Runtime", "LoadingPhase": "Default" } ],
	"Plugins": [ { "Name": "UE_MCP_Bridge", "Enabled": true } ]
}
"@ | Set-Content -Encoding utf8 (Join-Path $Dir "$Name.uproject")
    foreach ($t in @(@{ Suffix = ''; Type = 'Game' }, @{ Suffix = 'Editor'; Type = 'Editor' })) {
        $class = "$Name$($t.Suffix)Target"
        @"
using UnrealBuildTool;

public class $class : TargetRules
{
	public $class(TargetInfo Target) : base(Target)
	{
		Type = TargetType.$($t.Type);
		DefaultBuildSettings = BuildSettingsVersion.Latest;
		IncludeOrderVersion = EngineIncludeOrderVersion.Latest;
		ExtraModuleNames.Add("$Name");
	}
}
"@ | Set-Content -Encoding utf8 (Join-Path $Dir "Source\$Name$($t.Suffix).Target.cs")
    }
    @"
using UnrealBuildTool;

public class $Name : ModuleRules
{
	public $Name(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;
		PublicDependencyModuleNames.AddRange(new string[] { "Core", "CoreUObject", "Engine" });
	}
}
"@ | Set-Content -Encoding utf8 (Join-Path $Dir "Source\$Name\$Name.Build.cs")
    "#include `"Modules/ModuleManager.h`"`r`n`r`nIMPLEMENT_PRIMARY_GAME_MODULE(FDefaultGameModuleImpl, $Name, `"$Name`");" |
        Set-Content -Encoding utf8 (Join-Path $Dir "Source\$Name\$Name.cpp")
}

$results = @()
foreach ($engine in $engines) {
    $name = 'c' + ($engine.Label -replace '[^A-Za-z0-9]', '')
    $dir = Join-Path $WorkDir $name
    $association = if ($EngineRoot) { (Resolve-Path $EngineRoot).Path } else { $engine.Version }
    New-CompatProject $dir $name $association
    # Keep Binaries/Intermediate between runs so a re-check is incremental.
    robocopy $PluginDir (Join-Path $dir 'Plugins\UE_MCP_Bridge') /MIR /XD Binaries Intermediate /NFL /NDL /NJH /NJS /NP | Out-Null

    $log = Join-Path $dir 'build.log'
    Write-Host "Building against UE $($engine.Label) ($($engine.Root))..."
    $started = Get-Date
    & (Join-Path $engine.Root 'Engine\Build\BatchFiles\Build.bat') "${name}Editor" Win64 Development "-Project=$dir\$name.uproject" -WaitMutex -NoHotReloadFromIDE "-Log=$dir\ubt.txt" *> $log
    $code = $LASTEXITCODE
    $errors = @(Select-String -Path $log -Pattern ': (fatal )?error |error LNK|Unable to find plugin' | ForEach-Object { $_.Line.Trim() } | Select-Object -Unique)
    $results += [pscustomobject]@{
        Engine  = $engine.Label
        Result  = if ($code -eq 0) { 'OK' } else { 'FAILED' }
        Errors  = $errors.Count
        Minutes = [math]::Round(((Get-Date) - $started).TotalMinutes, 1)
        Log     = $log
    }
    if ($code -ne 0) {
        Write-Host "  First errors:"
        $errors | Select-Object -First 15 | ForEach-Object { Write-Host "    $_" }
    }
}

$results | Format-Table -AutoSize | Out-String | Write-Host
if ($results | Where-Object Result -ne 'OK') { exit 1 }
exit 0
