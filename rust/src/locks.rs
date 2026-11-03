//! Per-asset ownership across agents.
//!
//! The plugin keeps the authoritative lock registry. Every agent reaches the
//! editor through this hub, so the hub also mirrors the locks it granted and
//! refuses another session's mutation of a locked asset without a round trip
//! to the editor. The upstream daemon used one lock owner per editor, which
//! made all its agents the same owner; here each MCP session is its own.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

#[derive(Debug, Clone)]
struct Held {
    owner: String,
    expires: Instant,
}

#[derive(Default)]
pub struct LockTable {
    held: Mutex<HashMap<String, Held>>,
}

/// Keys whose string value names an in-editor asset the call would write.
const PATH_KEYS: &[&str] = &[
    "assetPath", "path", "blueprintPath", "sourcePath", "destinationPath", "targetPath", "materialPath", "table",
];
const PATH_LIST_KEYS: &[&str] = &["assetPaths", "paths"];

pub fn normalize(path: &str) -> String {
    let p = path.trim().replace('\\', "/");
    // /Game/X/Asset.Asset and /Game/X/Asset name the same package.
    let p = match p.rsplit_once('/') {
        Some((dir, leaf)) => match leaf.split_once('.') {
            Some((pkg, _)) => format!("{dir}/{pkg}"),
            None => p.clone(),
        },
        None => p.clone(),
    };
    p.to_lowercase()
}

/// Asset paths a call's parameters point at.
pub fn asset_paths(obj: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    let looks_like_asset = |s: &str| s.starts_with('/') && s.len() > 1 && !s.contains(':');
    for key in PATH_KEYS {
        if let Some(s) = obj.get(*key).and_then(Value::as_str) {
            if looks_like_asset(s) {
                out.push(normalize(s));
            }
        }
    }
    for key in PATH_LIST_KEYS {
        if let Some(list) = obj.get(*key).and_then(Value::as_array) {
            out.extend(list.iter().filter_map(Value::as_str).filter(|s| looks_like_asset(s)).map(normalize));
        }
    }
    out.sort();
    out.dedup();
    out
}

impl LockTable {
    pub fn record(&self, path: &str, owner: &str, ttl: Duration) {
        self.held.lock().unwrap().insert(normalize(path), Held { owner: owner.to_string(), expires: Instant::now() + ttl });
    }

    pub fn release(&self, path: &str, owner: &str, force: bool) {
        let mut held = self.held.lock().unwrap();
        let key = normalize(path);
        if held.get(&key).is_some_and(|h| force || h.owner == owner) {
            held.remove(&key);
        }
    }

    pub fn release_owner(&self, owner: &str) -> usize {
        let mut held = self.held.lock().unwrap();
        let before = held.len();
        held.retain(|_, h| h.owner != owner);
        before - held.len()
    }

    /// The first path in `paths` another live owner holds.
    pub fn conflict(&self, paths: &[String], owner: &str) -> Option<Value> {
        let mut held = self.held.lock().unwrap();
        let now = Instant::now();
        held.retain(|_, h| h.expires > now);
        paths.iter().find_map(|p| {
            held.get(p).filter(|h| h.owner != owner).map(|h| {
                json!({
                    "assetPath": p,
                    "holder": { "sessionId": h.owner, "ttlSecondsRemaining": h.expires.saturating_duration_since(now).as_secs() },
                })
            })
        })
    }

    pub fn snapshot(&self) -> Vec<Value> {
        let now = Instant::now();
        self.held
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, h)| h.expires > now)
            .map(|(p, h)| json!({ "assetPath": p, "sessionId": h.owner, "ttlSecondsRemaining": h.expires.duration_since(now).as_secs() }))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_path_and_package_path_are_one_lock() {
        let t = LockTable::default();
        t.record("/Game/BP/BP_Door.BP_Door", "a", Duration::from_secs(60));
        let paths = asset_paths(json!({ "assetPath": "/Game/BP/BP_Door" }).as_object().unwrap());
        assert!(t.conflict(&paths, "b").is_some());
        assert!(t.conflict(&paths, "a").is_none());
        t.release("/game/bp/bp_door", "a", false);
        assert!(t.conflict(&paths, "b").is_none());
    }

    #[test]
    fn filesystem_paths_are_not_assets() {
        assert!(asset_paths(json!({ "path": "C:/Temp/x.fbx" }).as_object().unwrap()).is_empty());
    }
}
