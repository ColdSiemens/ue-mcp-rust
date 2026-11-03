//! Parse an export_nodes_t3d blob (the same text as a Ctrl+C of graph nodes) into node records, and merge
//! read_blueprint_graph info (titles, literal text values) into them. Port of `scripts/lib/t3d.mjs` and the pure
//! parts of `scripts/bp_layout.mjs`.

use std::collections::HashMap;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;

use super::js;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub node: String,
    /// The linked pin's PinId ("" when the T3D names none).
    pub pin: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinDir {
    In,
    Out,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    /// PinId, raw ("" when absent).
    pub id: String,
    /// PinName, unquoted ("" when absent).
    pub name: String,
    /// PinFriendlyName, raw (e.g. `NSLOCTEXT("K2Node", "Target", "Target")`).
    pub friendly: Option<String>,
    pub dir: PinDir,
    pub cat: String,
    /// PinType.PinSubCategoryObject, raw; "None" when absent.
    pub sub_obj: String,
    /// PinType.ContainerType; "None" when absent.
    pub container: String,
    /// DefaultValue, unquoted.
    pub def: Option<String>,
    /// DefaultTextValue, raw.
    pub def_text: Option<String>,
    /// The literal text value read_blueprint_graph reports (`defaultTextValue`), set by `apply_graph_info`.
    pub text: Option<String>,
    pub hidden: bool,
    pub adv: bool,
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub name: String,
    /// Class name without its package ("K2Node_CallFunction").
    pub cls: String,
    pub guid: Option<String>,
    pub x: f64,
    pub y: f64,
    /// Every other `Key=Value` line of the node, raw.
    pub props: IndexMap<String, String>,
    pub pins: Vec<Pin>,
    /// Title from read_blueprint_graph, set by `apply_graph_info`.
    pub title: Option<String>,
}

/// `a=1,b=(x,y),c="q,("` -> [(a,1),(b,(x,y)),(c,"q,(")], honouring quotes and nested parens.
fn split_pairs(s: &str) -> Vec<(String, String)> {
    let b = s.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && (b[i] == b',' || b[i] == b' ') {
            i += 1;
        }
        if i >= n {
            break;
        }
        let Some(rel) = s[i..].find('=') else { break };
        let eq = i + rel;
        let key = js::trim(&s[i..eq]).to_string();
        let mut j = eq + 1;
        let mut depth = 0i64;
        let mut inq = false;
        while j < n {
            let ch = b[j];
            if inq {
                if ch == b'\\' {
                    j += 2;
                    continue;
                }
                if ch == b'"' {
                    inq = false;
                }
                j += 1;
                continue;
            }
            if ch == b'"' {
                inq = true;
            } else if ch == b'(' {
                depth += 1;
            } else if ch == b')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            } else if ch == b',' && depth == 0 {
                break;
            }
            j += 1;
        }
        out.push((key, s[eq + 1..j.min(n)].to_string()));
        i = j + 1;
    }
    out
}

/// Strip one level of `"..."` quoting (after trimming) and unescape `\"` and `\\`.
pub fn unq(v: &str) -> String {
    let v = js::trim(v);
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        v.to_string()
    }
}

/// `LinkedTo=(K2Node_X_3 GUID,K2Node_Y_1 GUID,)`
fn parse_links(v: Option<&String>) -> Vec<Link> {
    let Some(v) = v.filter(|v| !v.is_empty()) else { return Vec::new() };
    let t = js::trim(v);
    let t = t.strip_prefix('(').unwrap_or(t);
    let t = t.strip_suffix(')').unwrap_or(t);
    t.split(',')
        .map(js::trim)
        .filter(|x| !x.is_empty())
        .map(|x| {
            let mut parts = x.split(js::is_ws).filter(|p| !p.is_empty());
            Link { node: parts.next().unwrap_or("").to_string(), pin: parts.next().unwrap_or("").to_string() }
        })
        .collect()
}

fn parse_pin(body: &str) -> Pin {
    let kv: HashMap<String, String> = split_pairs(body).into_iter().collect();
    let get = |k: &str| kv.get(k);
    let or_none = |k: &str| get(k).filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| "None".to_string());
    Pin {
        id: get("PinId").cloned().unwrap_or_default(),
        name: get("PinName").map(|v| unq(v)).unwrap_or_default(),
        friendly: get("PinFriendlyName").filter(|v| !v.is_empty()).cloned(),
        dir: match get("Direction") {
            Some(d) if !d.is_empty() && unq(d) == "EGPD_Output" => PinDir::Out,
            _ => PinDir::In,
        },
        cat: get("PinType.PinCategory").map(|v| unq(v)).unwrap_or_default(),
        sub_obj: or_none("PinType.PinSubCategoryObject"),
        container: or_none("PinType.ContainerType"),
        def: get("DefaultValue").map(|v| unq(v)),
        def_text: get("DefaultTextValue").cloned(),
        text: None,
        hidden: get("bHidden").is_some_and(|v| v == "True"),
        adv: get("bAdvancedView").is_some_and(|v| v == "True"),
        links: parse_links(get("LinkedTo")),
    }
}

static RE_CLASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("Class=([^{}]+)", js::WS_CLASS)).unwrap());
static RE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"Name="([^"]+)""#).unwrap());

const PIN_PREFIX: &str = "CustomProperties Pin (";

pub fn parse_t3d(text: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut depth = 0i64;
    let mut cur: Option<Node> = None;
    for raw in text.split('\n') {
        let line = js::trim(raw.strip_suffix('\r').unwrap_or(raw));
        if line.starts_with("Begin Object") {
            depth += 1;
            if depth == 1 {
                let cls = RE_CLASS.captures(line).map_or("", |c| c.get(1).unwrap().as_str());
                let name = RE_NAME.captures(line).map_or("", |c| c.get(1).unwrap().as_str());
                cur = Some(Node {
                    name: name.to_string(),
                    cls: cls.rsplit('.').next().unwrap_or("").to_string(),
                    guid: None,
                    x: 0.0,
                    y: 0.0,
                    props: IndexMap::new(),
                    pins: Vec::new(),
                    title: None,
                });
            }
            continue;
        }
        if line.starts_with("End Object") {
            if depth == 1 {
                if let Some(c) = cur.take() {
                    nodes.push(c);
                }
            }
            depth -= 1;
            continue;
        }
        if depth != 1 {
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if line.starts_with(PIN_PREFIX) {
            // line.slice(prefix.length, line.lastIndexOf(')')); with no ')' the end is -1, i.e. drop the last char
            let end = match line.rfind(')') {
                Some(e) => e,
                None => line.char_indices().last().map_or(0, |(i, _)| i),
            };
            let body = if end > PIN_PREFIX.len() { &line[PIN_PREFIX.len()..end] } else { "" };
            c.pins.push(parse_pin(body));
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let (k, v) = (&line[..eq], &line[eq + 1..]);
        match k {
            "NodePosX" => c.x = js::number(v),
            "NodePosY" => c.y = js::number(v),
            "NodeGuid" => c.guid = Some(v.to_string()),
            _ => {
                c.props.insert(k.to_string(), v.to_string());
            }
        }
    }
    nodes
}

/// Merge a read_blueprint_graph answer (`{"nodes":[{"id":GUID,"title":..,"pins":[{"name","direction","defaultTextValue"?}]}]}`,
/// or the bare node array) into parsed nodes: each node's title, and each pin's literal text value.
pub fn apply_graph_info(nodes: &mut [Node], read_graph_result: &Value) {
    let empty = Vec::new();
    let list = read_graph_result
        .get("nodes")
        .and_then(Value::as_array)
        .or_else(|| read_graph_result.as_array())
        .unwrap_or(&empty);
    let mut info: HashMap<&str, &Value> = HashMap::new();
    for i in list {
        if let Some(id) = i.get("id").and_then(Value::as_str) {
            info.insert(id, i);
        }
    }
    for n in nodes.iter_mut() {
        let Some(i) = n.guid.as_deref().and_then(|g| info.get(g)) else { continue };
        n.title = i.get("title").and_then(Value::as_str).map(str::to_string);
        let ipins = i.get("pins").and_then(Value::as_array).unwrap_or(&empty);
        for p in n.pins.iter_mut() {
            let ip = ipins.iter().find(|q| {
                q.get("name").and_then(Value::as_str) == Some(p.name.as_str())
                    && (q.get("direction").and_then(Value::as_str) == Some("Output")) == (p.dir == PinDir::Out)
            });
            if let Some(t) = ip.and_then(|ip| ip.get("defaultTextValue")).and_then(Value::as_str) {
                p.text = Some(t.to_string());
            }
        }
    }
}

/// Classes that are not K2 nodes or comments, unique, in order of appearance: a graph with any is not a K2 graph.
pub fn foreign_classes(nodes: &[Node]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in nodes {
        if !(n.cls.starts_with("K2Node_") || n.cls == "EdGraphNode_Comment") && !out.contains(&n.cls) {
            out.push(n.cls.clone());
        }
    }
    out
}
