//! Project resolution and bridge port discovery.
//!
//! The editor plugin picks its port the same way on every launch, and the
//! client has to walk the same order or it ends up aimed at the wrong editor:
//! an explicit port, then the port the running editor published in
//! `Saved/UE_MCP_Bridge/port.json`, then the port derived from a hash of the
//! project root, then the historical default.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};

pub const DEFAULT_BRIDGE_PORT: u16 = 9877;
const EPHEMERAL_BASE: u32 = 49152;
const EPHEMERAL_SPAN: u32 = 65535 - EPHEMERAL_BASE + 1;

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub uproject: Option<PathBuf>,
}

impl Project {
    /// Accepts a .uproject file or a directory holding one.
    pub fn resolve(arg: &Path) -> Result<Project> {
        let full = dunce_canonicalize(arg).with_context(|| format!("project path {} does not exist", arg.display()))?;
        if full.is_file() {
            if full.extension().and_then(|e| e.to_str()) != Some("uproject") {
                bail!("{} is not a .uproject file", full.display());
            }
            let root = full.parent().map(Path::to_path_buf).unwrap_or_default();
            return Ok(Project { root, uproject: Some(full) });
        }
        let uproject = std::fs::read_dir(&full)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("uproject"));
        if uproject.is_none() {
            bail!("no .uproject file in {}", full.display());
        }
        Ok(Project { root: full, uproject })
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join("Saved").join("UE_MCP_Bridge")
    }

    pub fn name(&self) -> Option<String> {
        self.uproject.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned())
    }
}

/// `std::fs::canonicalize` returns `\\?\C:\...` on Windows, which neither the
/// plugin's hash nor a human recognises.
fn dunce_canonicalize(p: &Path) -> std::io::Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    let s = c.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c,
    })
}

/// Must match `FMCPBridgeStateFiles::NormalizeProjectRoot` in the plugin.
pub fn normalize_project_root(dir: &Path) -> String {
    let mut s = dir.to_string_lossy().replace('\\', "/");
    while s.ends_with('/') {
        s.pop();
    }
    s.to_lowercase()
}

/// Must match `FMCPBridgeServer::DeriveProjectPort` in the plugin.
pub fn derive_project_port(root: &Path) -> u16 {
    let h = Sha1::digest(normalize_project_root(root).as_bytes());
    let v = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    (EPHEMERAL_BASE + (v % EPHEMERAL_SPAN)) as u16
}

/// Where to look for the editor, in order. Re-read on every reconnect, because
/// an editor restart can land on a different port.
#[derive(Debug, Clone)]
pub struct PortResolver {
    pub explicit: Option<u16>,
    pub project: Option<Project>,
}

impl PortResolver {
    pub fn candidates(&self) -> Vec<u16> {
        if let Some(p) = self.explicit {
            return vec![p];
        }
        let mut out = Vec::new();
        if let Some(project) = &self.project {
            if let Some(p) = read_port_file(&project.state_dir().join("port.json")) {
                out.push(p);
            }
            let derived = derive_project_port(&project.root);
            if !out.contains(&derived) {
                out.push(derived);
            }
        }
        if !out.contains(&DEFAULT_BRIDGE_PORT) {
            out.push(DEFAULT_BRIDGE_PORT);
        }
        out
    }
}

fn read_port_file(path: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("port")?.as_u64().and_then(|p| u16::try_from(p).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_the_plugin() {
        assert_eq!(normalize_project_root(Path::new(r"C:\Users\Me\MyGame\")), "c:/users/me/mygame");
    }

    #[test]
    fn derived_port_is_in_the_ephemeral_range_and_stable() {
        let a = derive_project_port(Path::new("C:/Projects/Game"));
        let b = derive_project_port(Path::new(r"c:\projects\game\"));
        assert_eq!(a, b);
        assert!(a >= 49152);
    }
}
