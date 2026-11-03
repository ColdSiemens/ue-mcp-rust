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

/// Path parameters that name a folder, a file on disk, an actor, or an asset
/// the write only reads from, never the asset it edits.
const NOT_TARGET_KEYS: &[&str] = &[
    "packagePath", "package_path", "folderPath", "folder_path", "filePath", "fbx_file_path", "actorPath",
    "skeletonPath", "skeletalMeshPath", "meshPath", "animSequencePath", "anim_sequence_path",
];

/// The asset a write edits, by the convention its signature follows: the
/// first path parameter, e.g. `systemPath` in `set_module_input(systemPath, ...)`.
/// None when that first one is a folder, file, actor or read-only input.
pub fn primary_target_key(signature: &str) -> Option<&str> {
    let args = signature.split_once('(')?.1;
    let is_path = |w: &&str| ["Path", "_path", "Paths", "_paths"].iter().any(|s| w.ends_with(s));
    let first = args.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).find(is_path)?;
    (!NOT_TARGET_KEYS.contains(&first)).then_some(first)
}

/// Every asset a write's lock check covers: the well-known path keys plus the
/// action's own primary target.
pub fn write_targets(obj: &Map<String, Value>, signature: Option<&str>) -> Vec<String> {
    let mut out = asset_paths(obj);
    if let Some(key) = signature.and_then(primary_target_key).filter(|k| !PATH_KEYS.contains(k) && !PATH_LIST_KEYS.contains(k)) {
        let mut extra = Map::new();
        if let Some(v) = obj.get(key) {
            // Reuse the path checks by presenting the value under a known key.
            extra.insert(if v.is_array() { "paths" } else { "path" }.into(), v.clone());
        }
        out.extend(asset_paths(&extra));
        out.sort();
        out.dedup();
    }
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

    #[test]
    fn a_writes_first_path_parameter_is_its_target() {
        assert_eq!(primary_target_key("set_module_input(systemPath, moduleName, inputName, value, emitterName?)"), Some("systemPath"));
        assert_eq!(primary_target_key("add_blend_sample(assetPath|path, animation, position?:o)"), Some("assetPath"));
        assert_eq!(primary_target_key("set_blend_sample(assetPath|path, any(position:o; x:n))"), Some("assetPath"));
        // Inputs, folders and actors are not what the write edits.
        assert_eq!(primary_target_key("create_anim_blueprint(name, skeletonPath, packagePath?)"), None);
        assert_eq!(primary_target_key("set_actor_material(actorPath, materialPath)"), None);
        assert_eq!(primary_target_key("create_montage(name, animSequencePath, packagePath?)"), None);
    }

    #[test]
    fn write_targets_add_the_primary_target() {
        let p = json!({ "systemPath": "/Game/FX/NS_Fire.NS_Fire", "moduleName": "Spawn" });
        assert_eq!(write_targets(p.as_object().unwrap(), Some("set_module_input(systemPath, moduleName)")), vec!["/game/fx/ns_fire"]);
        let p = json!({ "control_rig_asset_path": ["/Game/Rig/CR_A", "C:/x"] });
        assert_eq!(write_targets(p.as_object().unwrap(), Some("x(control_rig_asset_path)")), vec!["/game/rig/cr_a"]);
        let p = json!({ "skeletonPath": "/Game/Char/SK" });
        assert!(write_targets(p.as_object().unwrap(), Some("create_blendspace(name, skeletonPath)")).is_empty());
    }
}
