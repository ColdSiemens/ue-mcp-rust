//! The tool surface: 30-odd category tools, each with an `action` enum and an
//! `args` bag, taken from the upstream server's recorded `tools/list` and its
//! action registry (see tools/extract_surface.py).
//!
//! Actions the upstream server ran in Node are kept only where this hub
//! implements them (see `LOCAL_ACTIONS`); the rest are pruned from the enum
//! and the description so no agent is offered a call that cannot work.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

static SURFACE_JSON: &str = include_str!("../surface/surface.json");

/// Tools that were Node-only end to end.
const DROPPED_TOOLS: &[&str] = &["flow", "feedback", "plugins"];

/// Upstream Node-side actions this hub implements itself.
pub const LOCAL_ACTIONS: &[(&str, &str)] = &[
    ("asset", "search"),
    ("asset", "migrate"),
    ("asset", "lock"),
    ("asset", "unlock"),
    ("asset", "unlock_all"),
    ("blueprint", "author"),
    ("editor", "get_engine_state"),
    ("editor", "execute_python"),
    ("editor", "play_in_editor_ignore_blueprint_errors"),
    ("niagara", "batch"),
    ("pcg", "wait_for_generation"),
    ("project", "get_status"),
    ("project", "get_info"),
    ("project", "describe_action"),
    ("project", "list_available_actions"),
    ("project", "search_tools"),
    ("editor", "start_editor"),
    ("editor", "stop_editor"),
    ("editor", "restart_editor"),
    ("editor", "request_editor_shutdown"),
    ("editor", "build_project"),
    ("project", "build"),
    ("project", "read_config"),
    ("project", "search_config"),
    ("project", "list_config_tags"),
];

/// Signature lines rewritten because this hub's version takes different args.
const SIGNATURE_OVERRIDES: &[(&str, &str, &str)] = &[(
    "editor",
    "execute_python",
    "execute_python(code, captureLog?:b, maxLogChars?:n, resultVariable?) - last resort: prefer a dedicated action",
)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Read,
    Mutate,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum Dispatch {
    Bridge { method: String },
    Epic { toolset: String, tool: String, schema: Value },
    Local,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub name: String,
    pub dispatch: Dispatch,
    pub effect: Effect,
    pub summary: Option<String>,
    pub signature: Option<String>,
    pub timeout_ms: Option<u64>,
    pub follow_up: bool,
}

impl Action {
    pub fn bridge_method(&self) -> Option<&str> {
        match &self.dispatch {
            Dispatch::Bridge { method } => Some(method),
            Dispatch::Epic { .. } => Some("epic_call_tool"),
            Dispatch::Local => None,
        }
    }

    pub fn is_epic(&self) -> bool {
        matches!(self.dispatch, Dispatch::Epic { .. })
    }
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    /// The one-paragraph summary at the top of the upstream description.
    pub summary: String,
    /// Description lines after the summary, one per action where the upstream had one.
    header: String,
    pub actions: Vec<Action>,
    index: HashMap<String, usize>,
    schema_extra: Map<String, Value>,
}

impl Tool {
    pub fn action(&self, name: &str) -> Option<&Action> {
        self.index.get(name).map(|&i| &self.actions[i])
    }
}

pub struct Surface {
    pub tools: Vec<Tool>,
    index: HashMap<String, usize>,
    handler_specs: Map<String, Value>,
    handler_timeouts: HashMap<String, u64>,
}

/// Which slice of the surface one MCP session sees.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// Only these categories (plus `project`). None = all.
    pub tools: Option<HashSet<String>>,
    /// Hide the `epic_*` wrappers of Unreal's own toolsets.
    pub hide_epic: bool,
    /// Short descriptions: action names only, details via describe_action.
    pub compact: bool,
    /// Hide bridge actions the running plugin does not register.
    pub registered: Option<std::sync::Arc<HashSet<String>>>,
}

impl View {
    pub fn allows_tool(&self, name: &str) -> bool {
        // The epic category is the toolset registry itself; nothing in it works without one.
        if self.hide_epic && name == "epic" {
            return false;
        }
        name == "project" || self.tools.as_ref().is_none_or(|t| t.contains(name))
    }

    pub fn allows_action(&self, action: &Action) -> bool {
        if self.hide_epic && action.is_epic() {
            return false;
        }
        match (&self.registered, &action.dispatch) {
            (Some(reg), Dispatch::Bridge { method }) => reg.contains(method) || ALWAYS_SERVED.contains(&method.as_str()),
            (Some(reg), Dispatch::Epic { .. }) => reg.contains("epic_call_tool"),
            _ => true,
        }
    }
}

/// Answered by the plugin before its registry is consulted.
const ALWAYS_SERVED: &[&str] = &["get_engine_state", "get_bridge_capabilities"];

impl Surface {
    pub fn load() -> Surface {
        let mut raw: Value = serde_json::from_str(SURFACE_JSON).expect("embedded surface.json is valid");
        let handler_specs = raw.get_mut("handlerSpecs").map(Value::take).and_then(into_object).unwrap_or_default();
        let handler_timeouts = raw
            .get("handlerTimeoutSeconds")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| v.as_u64().map(|s| (k.clone(), s))).collect())
            .unwrap_or_default();
        let local: HashSet<(&str, &str)> = LOCAL_ACTIONS.iter().copied().collect();

        let mut tools = Vec::new();
        for t in raw.get_mut("tools").and_then(Value::as_array_mut).map(std::mem::take).unwrap_or_default() {
            let name = t["name"].as_str().unwrap_or_default().to_string();
            if DROPPED_TOOLS.contains(&name.as_str()) {
                continue;
            }
            let description = t["description"].as_str().unwrap_or_default();
            let (summary, header, lines) = split_description(description);

            let mut actions = Vec::new();
            for (aname, spec) in t["actions"].as_object().into_iter().flatten() {
                let kind = spec["kind"].as_str().unwrap_or("local");
                let dispatch = match kind {
                    "bridge" => Dispatch::Bridge { method: spec["bridge"].as_str().unwrap_or_default().to_string() },
                    "epic" => Dispatch::Epic {
                        toolset: spec["toolset"].as_str().unwrap_or_default().to_string(),
                        tool: spec["tool"].as_str().unwrap_or_default().to_string(),
                        schema: spec["schema"].clone(),
                    },
                    _ if local.contains(&(name.as_str(), aname.as_str())) => Dispatch::Local,
                    _ => continue,
                };
                let signature = SIGNATURE_OVERRIDES
                    .iter()
                    .find(|(t, a, _)| *t == name && a == aname)
                    .map(|(_, _, s)| s.to_string())
                    .or_else(|| lines.get(aname.as_str()).cloned());
                actions.push(Action {
                    name: aname.clone(),
                    dispatch,
                    effect: match spec["effect"].as_str() {
                        Some("read") => Effect::Read,
                        Some("mutate") => Effect::Mutate,
                        _ => Effect::Unknown,
                    },
                    summary: spec["summary"].as_str().map(str::to_string),
                    signature,
                    timeout_ms: spec["timeoutMs"].as_u64(),
                    follow_up: spec["followUp"].as_bool().unwrap_or(false),
                });
            }
            let index = actions.iter().enumerate().map(|(i, a)| (a.name.clone(), i)).collect();
            let mut schema_extra = t["inputSchema"]["properties"].as_object().cloned().unwrap_or_default();
            schema_extra.remove("action");
            tools.push(Tool { name, summary, header, actions, index, schema_extra });
        }
        let index = tools.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();
        Surface { tools, index, handler_specs, handler_timeouts }
    }

    pub fn tool(&self, name: &str) -> Option<&Tool> {
        self.index.get(name).map(|&i| &self.tools[i])
    }

    pub fn action_count(&self) -> usize {
        self.tools.iter().map(|t| t.actions.len()).sum()
    }

    pub fn handler_timeout_secs(&self, method: &str) -> Option<u64> {
        self.handler_timeouts.get(method).copied()
    }

    pub fn handler_spec(&self, method: &str) -> Option<&Value> {
        self.handler_specs.get(method)
    }

    /// The `tools/list` entries for one session's view.
    pub fn list(&self, view: &View) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|t| view.allows_tool(&t.name))
            .filter_map(|t| {
                let actions: Vec<&Action> = t.actions.iter().filter(|a| view.allows_action(a)).collect();
                if actions.is_empty() {
                    return None;
                }
                let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
                let description = if view.compact {
                    format!(
                        "{}\n\nCall {}(action, args={{...}}). Actions: {}.\nParameters: project(action=\"describe_action\", args={{name:\"{}.<action>\"}}).",
                        t.summary,
                        t.name,
                        names.join(", "),
                        t.name
                    )
                } else {
                    let mut d = format!("{}\n\n{}", t.summary, t.header);
                    for a in &actions {
                        d.push('\n');
                        d.push_str(a.signature.as_deref().unwrap_or(&a.name));
                    }
                    d
                };
                let mut properties = Map::new();
                // Compact mode names the actions once, in the description; the
                // enum would repeat every name.
                let action_schema = if view.compact {
                    json!({ "type": "string", "description": "One of the actions listed in the tool description." })
                } else {
                    json!({ "type": "string", "enum": names, "description": "The operation to run." })
                };
                properties.insert("action".into(), action_schema);
                for (k, v) in &t.schema_extra {
                    properties.insert(k.clone(), v.clone());
                }
                Some(json!({
                    "name": t.name,
                    "description": description,
                    "inputSchema": {
                        "type": "object",
                        "properties": properties,
                        "required": ["action"],
                        "additionalProperties": true,
                    },
                }))
            })
            .collect()
    }

    /// Resolve `tool.action`, `tool(action)`, or a bare action name with an optional category.
    pub fn find(&self, name: &str, category: Option<&str>) -> Vec<(&Tool, &Action)> {
        let name = name.trim();
        let (tool, action) = if let Some((t, rest)) = name.split_once('(') {
            (Some(t.trim()), rest.trim_end_matches(')').trim())
        } else if let Some((t, a)) = name.split_once('.') {
            (Some(t.trim()), a.trim())
        } else {
            (category, name)
        };
        let action = action.trim_matches('"');
        self.tools
            .iter()
            .filter(|t| tool.is_none_or(|n| n == t.name))
            .filter_map(|t| t.action(action).map(|a| (t, a)))
            .collect()
    }

    pub fn describe(&self, tool: &Tool, action: &Action) -> Value {
        let mut out = json!({
            "tool": tool.name,
            "action": action.name,
            "call": format!("{}(action=\"{}\", args={{...}})", tool.name, action.name),
            "effect": match action.effect { Effect::Read => "read", Effect::Mutate => "mutate", Effect::Unknown => "unknown" },
        });
        if let Some(sig) = &action.signature {
            out["signature"] = json!(sig);
        }
        if let Some(summary) = &action.summary {
            out["description"] = json!(summary);
        }
        match &action.dispatch {
            Dispatch::Bridge { method } => {
                out["bridgeMethod"] = json!(method);
                if let Some(spec) = self.handler_spec(method) {
                    if let Some(p) = spec.get("params") {
                        out["params"] = p.clone();
                    }
                    if let Some(c) = spec.get("choices") {
                        out["choices"] = c.clone();
                    }
                }
            }
            Dispatch::Epic { tool: epic_tool, schema, .. } => {
                out["bridgeMethod"] = json!("epic_call_tool");
                out["epicTool"] = json!(epic_tool);
                out["inputSchema"] = schema.clone();
                out["note"] = json!("Pass the tool's arguments flat in args, or whole as args.input; asset references accept a plain path.");
            }
            Dispatch::Local => {
                out["handledBy"] = json!("ue-mcp-rust hub");
            }
        }
        if let Some(ms) = action.timeout_ms {
            out["defaultTimeoutMs"] = json!(ms);
        }
        out
    }

    /// Keyword search over action names, signatures and descriptions.
    pub fn search(&self, query: &str, view: &View, limit: usize) -> Vec<Value> {
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 1)
            .map(str::to_lowercase)
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(f64, &Tool, &Action)> = Vec::new();
        for t in self.tools.iter().filter(|t| view.allows_tool(&t.name)) {
            for a in t.actions.iter().filter(|a| view.allows_action(a)) {
                let name = a.name.to_lowercase();
                let body = format!(
                    "{} {} {}",
                    t.name,
                    a.signature.as_deref().unwrap_or(""),
                    a.summary.as_deref().unwrap_or("")
                )
                .to_lowercase();
                let mut score = 0.0;
                for term in &terms {
                    if name.split('_').any(|p| p == term) {
                        score += 3.0;
                    } else if name.contains(term.as_str()) {
                        score += 2.0;
                    }
                    if body.contains(term.as_str()) {
                        score += 1.0;
                    }
                }
                if a.is_epic() {
                    score *= 0.8;
                }
                if score > 0.0 {
                    scored.push((score, t, a));
                }
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(limit)
            .map(|(score, t, a)| {
                json!({
                    "call": format!("{}.{}", t.name, a.name),
                    "signature": a.signature,
                    "score": (score * 10.0).round() / 10.0,
                })
            })
            .collect()
    }
}

fn into_object(v: Value) -> Option<Map<String, Value>> {
    match v {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// Upstream descriptions read: summary paragraph, blank line, a "Call x(...).
/// Actions:" header, then one signature line per action.
fn split_description(d: &str) -> (String, String, HashMap<String, String>) {
    let (summary, rest) = d.split_once("\n\n").unwrap_or((d, ""));
    let mut header = String::new();
    let mut lines = HashMap::new();
    for line in rest.lines() {
        let name: String = line.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !name.is_empty() && line[name.len()..].starts_with('(') {
            lines.insert(name, line.to_string());
        } else if lines.is_empty() {
            if !header.is_empty() {
                header.push('\n');
            }
            header.push_str(line);
        }
    }
    (summary.trim().to_string(), header, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_loads_and_prunes_node_only_actions() {
        let s = Surface::load();
        assert!(s.tool("flow").is_none());
        let editor = s.tool("editor").unwrap();
        assert!(editor.action("checkpoint").is_none(), "unported local action must be pruned");
        assert!(matches!(editor.action("execute_python").unwrap().dispatch, Dispatch::Local));
        let level = s.tool("level").unwrap();
        assert!(matches!(level.action("get_outliner").unwrap().dispatch, Dispatch::Bridge { .. }));
        assert!(s.action_count() > 1900);
    }

    #[test]
    fn find_accepts_all_spellings() {
        let s = Surface::load();
        assert_eq!(s.find("level.place_actor", None).len(), 1);
        assert_eq!(s.find("level(place_actor)", None).len(), 1);
        assert_eq!(s.find("place_actor", Some("level")).len(), 1);
    }

    #[test]
    fn compact_view_is_much_smaller() {
        let s = Surface::load();
        let full = serde_json::to_string(&s.list(&View::default())).unwrap().len();
        let compact = serde_json::to_string(&s.list(&View { compact: true, ..Default::default() })).unwrap().len();
        let lean = serde_json::to_string(&s.list(&View { compact: true, hide_epic: true, ..Default::default() })).unwrap().len();
        assert!(compact * 3 < full, "compact {compact} vs full {full}");
        assert!(lean < compact, "lean {lean} vs compact {compact}");
    }

    #[test]
    fn category_filter_keeps_project() {
        let s = Surface::load();
        let view = View { tools: Some(["level".to_string()].into_iter().collect()), ..Default::default() };
        let mut names: Vec<String> = s.list(&view).iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["level", "project"]);
    }
}
