//! Editor lifecycle and C++ builds: the actions that need the operating
//! system rather than the editor, so the plugin cannot serve them.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::Project;
use crate::dispatch::Hub;

const DEFAULT_START_TIMEOUT_SECS: f64 = 300.0;
const STOP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a new session gives its sockets to find an editor that is already running.
const EXISTING_EDITOR_WAIT: Duration = Duration::from_secs(3);
const DEFAULT_BUILD_TIMEOUT_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct Engine {
    pub root: PathBuf,
    pub editor: PathBuf,
    /// Which rule found it, for the answer.
    pub source: String,
}

impl Engine {
    fn from_root(root: PathBuf, source: String) -> Engine {
        let editor = root.join(EDITOR_REL);
        Engine { root, editor, source }
    }

    pub fn build_script(&self) -> PathBuf {
        self.root.join(BUILD_REL)
    }
}

#[cfg(windows)]
const EDITOR_REL: &str = "Engine/Binaries/Win64/UnrealEditor.exe";
#[cfg(target_os = "macos")]
const EDITOR_REL: &str = "Engine/Binaries/Mac/UnrealEditor.app/Contents/MacOS/UnrealEditor";
#[cfg(all(unix, not(target_os = "macos")))]
const EDITOR_REL: &str = "Engine/Binaries/Linux/UnrealEditor";

#[cfg(windows)]
const BUILD_REL: &str = "Engine/Build/BatchFiles/Build.bat";
#[cfg(target_os = "macos")]
const BUILD_REL: &str = "Engine/Build/BatchFiles/Mac/Build.sh";
#[cfg(all(unix, not(target_os = "macos")))]
const BUILD_REL: &str = "Engine/Build/BatchFiles/Linux/Build.sh";

#[cfg(windows)]
const DEFAULT_PLATFORM: &str = "Win64";
#[cfg(target_os = "macos")]
const DEFAULT_PLATFORM: &str = "Mac";
#[cfg(all(unix, not(target_os = "macos")))]
const DEFAULT_PLATFORM: &str = "Linux";

/// Find the engine for a project, in the order a person would check:
/// UE_EDITOR_PATH, --engine-root / UE_ENGINE_ROOT, then the .uproject's
/// EngineAssociation (a launcher version, a registered source build, or a path).
pub fn resolve_engine(project: &Project, explicit_root: Option<&Path>) -> Result<Engine, String> {
    if let Ok(exe) = std::env::var("UE_EDITOR_PATH") {
        let editor = PathBuf::from(exe);
        // <root>/Engine/Binaries/<Platform>/UnrealEditor.exe
        let root = editor.ancestors().nth(4).map(Path::to_path_buf).unwrap_or_default();
        return Ok(Engine { root, editor, source: "UE_EDITOR_PATH".into() });
    }
    let mut tried = Vec::new();
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    if let Some(root) = explicit_root {
        candidates.push((root.to_path_buf(), "--engine-root".into()));
    }
    let association = project
        .uproject
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("EngineAssociation").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    if association.starts_with('{') {
        if let Some(dir) = registry_value(r"HKCU\Software\Epic Games\Unreal Engine\Builds", &association) {
            candidates.push((PathBuf::from(dir), format!("source build {association}")));
        }
    } else if association.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        let key = format!(r"HKLM\SOFTWARE\EpicGames\Unreal Engine\{association}");
        if let Some(dir) = registry_value(&key, "InstalledDirectory") {
            candidates.push((PathBuf::from(dir), format!("UE {association} (launcher)")));
        }
        candidates.push((PathBuf::from(format!(r"C:\Program Files\Epic Games\UE_{association}")), format!("UE {association} default location")));
    } else if !association.is_empty() {
        candidates.push((project.root.join(&association), "EngineAssociation path".into()));
    }
    // A project that lives inside a source build's tree.
    if let Some(root) = project.root.ancestors().skip(1).find(|a| a.join(EDITOR_REL).exists()) {
        candidates.push((root.to_path_buf(), "engine tree around the project".into()));
    }
    for (root, source) in candidates {
        let engine = Engine::from_root(root, source);
        if engine.editor.exists() {
            return Ok(engine);
        }
        tried.push(format!("{} ({})", engine.editor.display(), engine.source));
    }
    Err(format!(
        "No Unreal Editor found for EngineAssociation '{association}'. Tried: {}. Pass --engine-root <engine folder> or set UE_EDITOR_PATH.",
        if tried.is_empty() { "nothing".to_string() } else { tried.join("; ") }
    ))
}

#[cfg(windows)]
fn registry_value(key: &str, name: &str) -> Option<String> {
    let out = std::process::Command::new("reg").args(["query", key, "/v", name]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix(name)?.trim_start();
        let value = rest.strip_prefix("REG_SZ").or_else(|| rest.strip_prefix("REG_EXPAND_SZ"))?;
        Some(value.trim().to_string())
    })
}

#[cfg(not(windows))]
fn registry_value(_key: &str, _name: &str) -> Option<String> {
    None
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

/// `<Name>Editor` from Source/*Editor.Target.cs, else the project name.
fn editor_target(project: &Project) -> Option<String> {
    let from_source = std::fs::read_dir(project.root.join("Source")).ok().and_then(|dir| {
        dir.filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .find(|n| n.ends_with("Editor.Target.cs"))
            .map(|n| n.trim_end_matches(".Target.cs").to_string())
    });
    from_source.or_else(|| project.name().map(|n| format!("{n}Editor")))
}

impl Hub {
    fn project_and_engine(&self) -> Result<(&Project, PathBuf, Engine), String> {
        let project = self.project.as_ref().ok_or("No project: start ue-mcp-rs with the .uproject path.")?;
        let uproject = project.uproject.clone().ok_or("No .uproject file found in the project folder.")?;
        let engine = resolve_engine(project, self.opts.engine_root.as_deref())?;
        Ok((project, uproject, engine))
    }

    pub async fn start_editor(&self, p: &Map<String, Value>) -> Result<Value, String> {
        // A new session's sockets may still be connecting to an editor that is
        // already up; launching then would open the project twice.
        if self.bridge.wait_connected(EXISTING_EDITOR_WAIT).await {
            return Ok(json!({
                "success": false,
                "alreadyRunning": true,
                "bridgeReady": true,
                "message": "An editor for this project is already connected; nothing was started.",
            }));
        }
        let (project, uproject, engine) = self.project_and_engine()?;
        let timeout = Duration::from_secs_f64(p.get("timeout").and_then(Value::as_f64).unwrap_or(DEFAULT_START_TIMEOUT_SECS).clamp(10.0, 3600.0));

        let mut cmd = std::process::Command::new(&engine.editor);
        cmd.arg(&uproject).args(&self.opts.editor_args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(policy) = p.get("dialogPolicy").and_then(Value::as_str) {
            cmd.env("UE_MCP_DIALOG_POLICY", policy);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            // The editor outlives this server.
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        let mut child = cmd.spawn().map_err(|e| format!("could not launch {}: {e}", engine.editor.display()))?;
        let pid = child.id();
        let started = Instant::now();
        let log = project.root.join("Saved").join("Logs").join(format!("{}.log", project.name().unwrap_or_default()));

        let mut last_state = Value::Null;
        while started.elapsed() < timeout {
            if let Ok(Some(status)) = child.try_wait() {
                return Ok(json!({
                    "success": false,
                    "pid": pid,
                    "exitCode": status.code(),
                    "message": format!("The editor exited during startup. Its log: {}", log.display()),
                }));
            }
            if self.bridge.is_connected() {
                if let Ok(state) = self.bridge.call("get_engine_state", json!({}), None, Duration::from_secs(5)).await {
                    if state.get("phase").and_then(Value::as_str) == Some("ready") {
                        return Ok(json!({
                            "success": true,
                            "pid": pid,
                            "seconds": started.elapsed().as_secs(),
                            "editor": engine.editor,
                            "engineFrom": engine.source,
                            "bridge": self.bridge.status(),
                        }));
                    }
                    last_state = state;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(json!({
            "success": false,
            "timedOut": true,
            "pid": pid,
            "seconds": started.elapsed().as_secs(),
            "lastEngineState": last_state,
            "message": format!(
                "The editor is still starting after {}s (shader compiles on a first launch can take much longer). It keeps running; call editor(get_engine_state) to follow it. Log: {}",
                timeout.as_secs(),
                log.display()
            ),
        }))
    }

    pub async fn stop_editor(&self) -> Result<Value, String> {
        if !self.bridge.wait_connected(EXISTING_EDITOR_WAIT).await {
            return Ok(json!({ "success": false, "alreadyStopped": true, "message": "No editor of this project is connected." }));
        }
        let pid = self
            .bridge
            .call("get_engine_state", json!({}), None, Duration::from_secs(5))
            .await
            .ok()
            .and_then(|s| s.get("pid").and_then(Value::as_f64))
            .map(|p| p as u32);
        // The editor decides: with requireClean=false it raises its own save
        // prompt for anything dirty rather than this server discarding work.
        let answer = self.bridge_call("request_editor_shutdown", json!({ "requireClean": false, "endPIE": true }), None).await?;
        if answer.get("success") == Some(&Value::Bool(false)) {
            return Ok(answer);
        }
        let started = Instant::now();
        while started.elapsed() < STOP_TIMEOUT {
            let process_gone = pid.is_none_or(|p| !pid_alive(p));
            if !self.bridge.is_connected() && process_gone {
                return Ok(json!({ "success": true, "stopped": { "pid": pid }, "seconds": started.elapsed().as_secs() }));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(json!({
            "success": false,
            "timedOut": true,
            "pid": pid,
            "message": "The editor accepted the quit but is still running. It is probably showing a save prompt: read it with editor(list_dialogs) and answer with editor(respond_to_dialog).",
        }))
    }

    pub async fn restart_editor(&self, p: &Map<String, Value>) -> Result<Value, String> {
        let stop = self.stop_editor().await?;
        let stopped = stop.get("success") == Some(&Value::Bool(true)) || stop.get("alreadyStopped") == Some(&Value::Bool(true));
        if !stopped {
            return Ok(json!({ "success": false, "stop": stop, "message": "The stop half did not finish, so nothing was started." }));
        }
        // The plugin's sockets close a moment before the port is free for the next editor.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let start = self.start_editor(p).await?;
        Ok(json!({ "success": start.get("success") == Some(&Value::Bool(true)), "stop": stop, "start": start }))
    }

    /// Build the project's editor target with UnrealBuildTool.
    pub async fn build_project(&self, p: &Map<String, Value>, timeout_ms: Option<u64>) -> Result<Value, String> {
        if self.bridge.wait_connected(EXISTING_EDITOR_WAIT).await {
            return Err(
                "The editor is running, and building while it is open fails on its locked DLLs. Stop it with editor(stop_editor) first, or compile in place with project(live_coding_compile)."
                    .into(),
            );
        }
        let (project, uproject, engine) = self.project_and_engine()?;
        let target = editor_target(project).ok_or("Could not work out the editor target name; the project has no Source/*Editor.Target.cs.")?;
        let configuration = p.get("configuration").and_then(Value::as_str).unwrap_or("Development").to_string();
        let platform = p.get("platform").and_then(Value::as_str).unwrap_or(DEFAULT_PLATFORM).to_string();
        let script = engine.build_script();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_BUILD_TIMEOUT_MS));
        let base_args = vec![
            target.clone(),
            platform.clone(),
            configuration.clone(),
            format!("-Project={}", uproject.display()),
            "-WaitMutex".to_string(),
            "-NoHotReloadFromIDE".to_string(),
        ];
        let started = Instant::now();
        if p.get("clean") == Some(&Value::Bool(true)) {
            let mut clean_args = base_args.clone();
            clean_args.push("-Clean".into());
            let clean = run_build(&script, &clean_args, deadline).await?;
            if clean.code != Some(0) {
                return Ok(clean.report(false, &target, &configuration, &platform, started));
            }
        }
        let run = run_build(&script, &base_args, deadline).await?;
        let ok = run.code == Some(0);
        Ok(run.report(ok, &target, &configuration, &platform, started))
    }
}

struct BuildRun {
    code: Option<i32>,
    lines: Vec<String>,
    timed_out: bool,
}

impl BuildRun {
    fn report(&self, ok: bool, target: &str, configuration: &str, platform: &str, started: Instant) -> Value {
        let is_error = |l: &&String| {
            let lower = l.to_lowercase();
            lower.contains(": error ") || lower.contains(": fatal error") || lower.contains("error lnk") || lower.starts_with("error:")
        };
        let errors: Vec<&String> = self.lines.iter().filter(is_error).take(50).collect();
        let warnings = self.lines.iter().filter(|l| l.to_lowercase().contains(": warning ")).count();
        let tail: Vec<&String> = self.lines.iter().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect();
        json!({
            "success": ok,
            "exitCode": self.code,
            "timedOut": self.timed_out,
            "target": target,
            "configuration": configuration,
            "platform": platform,
            "seconds": started.elapsed().as_secs(),
            "errorCount": self.lines.iter().filter(is_error).count(),
            "warningCount": warnings,
            "errors": errors,
            "tail": tail,
        })
    }
}

async fn run_build(script: &Path, args: &[String], deadline: Instant) -> Result<BuildRun, String> {
    let mut child = tokio::process::Command::new(script)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", script.display()))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), child.stderr.take().map(|s| Box::new(s) as _)]
        .into_iter()
        .flatten()
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let (code, timed_out) = match tokio::time::timeout(remaining, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => return Err(format!("build process failed: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            (None, true)
        }
    };
    let mut lines = Vec::new();
    while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        lines.push(line);
    }
    Ok(BuildRun { code, lines, timed_out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_name_comes_from_the_editor_target_file() {
        let dir = std::env::temp_dir().join(format!("ue-mcp-rs-target-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Source")).unwrap();
        std::fs::write(dir.join("Game.uproject"), "{}").unwrap();
        std::fs::write(dir.join("Source").join("GameEditor.Target.cs"), "").unwrap();
        let project = Project::resolve(&dir).unwrap();
        assert_eq!(editor_target(&project).as_deref(), Some("GameEditor"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
