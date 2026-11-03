//! Project files read straight from disk, so they work with the editor closed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::Project;

const MAX_SEARCH_RESULTS: usize = 200;

fn ini_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("ini")) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// `Engine`, `DefaultEngine`, `DefaultEngine.ini` or `Windows/WindowsEngine.ini`.
fn resolve_config(dir: &Path, name: &str) -> Option<PathBuf> {
    let name = name.trim().trim_end_matches(".ini");
    [format!("{name}.ini"), format!("Default{name}.ini")]
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

/// Sections of key/value pairs. A key set more than once (the +Key= array
/// syntax) collects its values in order.
pub fn parse_ini(text: &str) -> Map<String, Value> {
    let mut sections = Map::new();
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current = line[1..line.len() - 1].to_string();
            sections.entry(current.clone()).or_insert_with(|| json!({}));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let section = sections.entry(current.clone()).or_insert_with(|| json!({}));
        let entries = section.as_object_mut().expect("sections are objects");
        let value = Value::String(value.trim().to_string());
        match entries.get_mut(key.trim()) {
            Some(Value::Array(a)) => a.push(value),
            Some(existing) => {
                let first = existing.take();
                *existing = json!([first, value]);
            }
            None => {
                entries.insert(key.trim().to_string(), value);
            }
        }
    }
    sections
}

pub fn read_config(project: &Project, name: &str) -> Result<Value, String> {
    let dir = project.root.join("Config");
    let path = resolve_config(&dir, name).ok_or_else(|| {
        let available: Vec<String> = ini_files(&dir)
            .iter()
            .filter_map(|p| p.strip_prefix(&dir).ok().map(|r| r.to_string_lossy().replace('\\', "/")))
            .collect();
        format!("No config '{name}' in {}. Available: {}", dir.display(), available.join(", "))
    })?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sections = parse_ini(&text);
    Ok(json!({ "path": path, "configName": name, "sectionCount": sections.len(), "sections": sections }))
}

pub fn search_config(project: &Project, query: &str) -> Result<Value, String> {
    let dir = project.root.join("Config");
    let needle = query.to_lowercase();
    let mut results = Vec::new();
    let mut total = 0usize;
    for file in ini_files(&dir) {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let rel = file.strip_prefix(&dir).unwrap_or(&file).to_string_lossy().replace('\\', "/");
        let mut section = String::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].to_string();
                continue;
            }
            if line.to_lowercase().contains(&needle) {
                total += 1;
                if results.len() < MAX_SEARCH_RESULTS {
                    results.push(json!({ "file": rel, "section": section, "line": i + 1, "content": line }));
                }
            }
        }
    }
    Ok(json!({ "query": query, "resultCount": total, "truncated": total > results.len(), "results": results }))
}

pub fn list_config_tags(project: &Project) -> Result<Value, String> {
    let mut tags = BTreeSet::new();
    for file in ini_files(&project.root.join("Config")) {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let mut in_tags = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') && line.ends_with(']') {
                in_tags = line.to_lowercase().contains("gameplaytag");
                continue;
            }
            if !in_tags {
                continue;
            }
            for marker in ["TagName=\"", "Tag=\""] {
                if let Some(rest) = line.split(marker).nth(1) {
                    if let Some(tag) = rest.split('"').next().filter(|t| !t.is_empty()) {
                        tags.insert(tag.to_string());
                    }
                    break;
                }
            }
        }
    }
    let mut tree = Map::new();
    for tag in &tags {
        let mut node = &mut tree;
        for part in tag.split('.') {
            node = node.entry(part.to_string()).or_insert_with(|| json!({})).as_object_mut().expect("tree nodes are objects");
        }
    }
    Ok(json!({ "source": "config_files", "count": tags.len(), "tags": tags, "tree": tree }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_keys_become_arrays() {
        let s = parse_ini("[/Script/Engine.Engine]\n+Paths=A\n+Paths=B\nName=X\n; comment\n");
        assert_eq!(s["/Script/Engine.Engine"]["+Paths"], json!(["A", "B"]));
        assert_eq!(s["/Script/Engine.Engine"]["Name"], "X");
    }

    #[test]
    fn tags_are_read_from_gameplay_tag_sections() {
        let dir = std::env::temp_dir().join(format!("ue-mcp-rs-tags-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Config")).unwrap();
        std::fs::write(dir.join("G.uproject"), "{}").unwrap();
        std::fs::write(
            dir.join("Config").join("DefaultGameplayTags.ini"),
            "[/Script/GameplayTags.GameplayTagsSettings]\n+GameplayTagList=(Tag=\"Ability.Fire\",DevComment=\"\")\n+GameplayTagList=(Tag=\"Ability.Ice\",DevComment=\"\")\n",
        )
        .unwrap();
        let project = Project::resolve(&dir).unwrap();
        let out = list_config_tags(&project).unwrap();
        assert_eq!(out["tags"], json!(["Ability.Fire", "Ability.Ice"]));
        assert!(out["tree"]["Ability"]["Fire"].is_object());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
