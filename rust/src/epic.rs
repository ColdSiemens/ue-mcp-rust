//! The `epic_call_tool` envelope for `epic_*` actions: Unreal's own editor
//! toolsets, wrapped by the plugin. Port of upstream `resolveEpicToolInput`.

use serde_json::{json, Map, Value};

fn is_asset_ref(prop: Option<&Value>) -> bool {
    prop.is_some_and(|p| {
        p.get("type").and_then(Value::as_str) == Some("object")
            && p.get("properties").and_then(Value::as_object).is_some_and(|m| m.contains_key("refPath"))
    })
}

/// `/Game/X/SG_A` -> `/Game/X/SG_A.SG_A`; anything already naming an object is unchanged.
pub fn expand_ref_path(value: &str) -> String {
    let path = value.trim().replace('\\', "/");
    if !path.starts_with('/') || path.contains(':') {
        return value.to_string();
    }
    let leaf = path.rsplit('/').next().unwrap_or("");
    if leaf.is_empty() || leaf.contains('.') || path.split('/').filter(|s| !s.is_empty()).count() < 2 {
        return value.to_string();
    }
    format!("{path}.{leaf}")
}

fn expand_ref_value(v: Value) -> Value {
    match v {
        Value::String(s) => json!({ "refPath": expand_ref_path(&s) }),
        Value::Object(mut m) => {
            if let Some(Value::String(r)) = m.get("refPath") {
                let expanded = expand_ref_path(r);
                m.insert("refPath".into(), Value::String(expanded));
            }
            Value::Object(m)
        }
        other => other,
    }
}

pub fn parse_objectish(v: &Value) -> Value {
    if let Value::String(s) = v {
        let t = s.trim();
        if t.starts_with('{') || t.starts_with('[') {
            if let Ok(parsed) = serde_json::from_str(t) {
                return parsed;
            }
        }
    }
    v.clone()
}

fn example_for(name: &str, prop: Option<&Value>) -> String {
    if is_asset_ref(prop) {
        return format!("\"{name}\": {{ \"refPath\": \"/Game/Path/Asset\" }}");
    }
    match prop.and_then(|p| p.get("type")).and_then(Value::as_str) {
        Some("number") | Some("integer") => format!("\"{name}\": 0"),
        Some("boolean") => format!("\"{name}\": false"),
        Some("array") => format!("\"{name}\": []"),
        Some("object") => format!("\"{name}\": {{}}"),
        _ => format!("\"{name}\": \"...\""),
    }
}

pub fn build_call(toolset: &str, tool: &str, schema: &Value, params: &Map<String, Value>) -> Result<Value, String> {
    let mut out = json!({ "toolset": toolset, "tool": tool });
    if let Some(s) = params.get("inputJson").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        out["inputJson"] = json!(s);
        return Ok(out);
    }
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        for key in ["input", "inputJson"] {
            if let Some(v) = params.get(key).filter(|v| !v.is_null()) {
                out[key] = v.clone();
            }
        }
        return Ok(out);
    };

    let mut input = match params.get("input").map(parse_objectish) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    for name in props.keys() {
        if input.get(name).is_some_and(|v| !v.is_null()) {
            continue;
        }
        match params.get(name) {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if s.is_empty() => {}
            Some(flat) => {
                input.insert(name.clone(), parse_objectish(flat));
            }
        }
    }

    // One unfilled asset reference and a category-style assetPath/path: that is the one.
    let unresolved: Vec<&String> = props.keys().filter(|n| !input.contains_key(*n) && is_asset_ref(props.get(*n))).collect();
    let canonical = ["assetPath", "path"]
        .iter()
        .find_map(|k| params.get(*k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()));
    if let (1, Some(path)) = (unresolved.len(), canonical) {
        input.insert(unresolved[0].clone(), json!(path));
    }

    for (name, prop) in props {
        let Some(v) = input.remove(name) else { continue };
        let v = if is_asset_ref(Some(prop)) {
            expand_ref_value(v)
        } else if prop.get("type").and_then(Value::as_str) == Some("array") && is_asset_ref(prop.get("items")) {
            match v {
                Value::Array(items) => Value::Array(items.into_iter().map(expand_ref_value).collect()),
                other => other,
            }
        } else {
            v
        };
        input.insert(name.clone(), v);
    }

    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let missing: Vec<&str> = required.iter().copied().filter(|n| !input.contains_key(*n)).collect();
    if !missing.is_empty() {
        let shape = missing.iter().map(|n| example_for(n, props.get(*n))).collect::<Vec<_>>().join(", ");
        let ignored: Vec<&String> = params
            .iter()
            .filter(|(k, v)| !props.contains_key(*k) && !["input", "inputJson", "assetPath", "path"].contains(&k.as_str()) && !v.is_null())
            .map(|(k, _)| k)
            .collect();
        let mut msg = format!("{tool} is missing required argument(s): {}. Pass them in 'input', for example {{\"input\": {{{shape}}}}}.", missing.join(", "));
        if !ignored.is_empty() {
            msg.push_str(&format!(
                " These parameters are not arguments of this tool and were not sent: {}.",
                ignored.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        return Err(msg);
    }
    out["input"] = Value::Object(input);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_bare_package_paths() {
        assert_eq!(expand_ref_path("/Game/X/SG_A"), "/Game/X/SG_A.SG_A");
        assert_eq!(expand_ref_path("/Game/X/SG_A.SG_A"), "/Game/X/SG_A.SG_A");
        assert_eq!(expand_ref_path("C:/x"), "C:/x");
    }

    #[test]
    fn builds_input_from_flat_params_and_asset_path() {
        let schema = json!({
            "properties": { "actor": { "type": "object", "properties": { "refPath": {} } }, "tag": { "type": "string" } },
            "required": ["actor", "tag"]
        });
        let params = json!({ "assetPath": "/Game/Maps/Door", "tag": "open" });
        let call = build_call("ts", "ts.add_tag", &schema, params.as_object().unwrap()).unwrap();
        assert_eq!(call["input"]["actor"]["refPath"], "/Game/Maps/Door.Door");
        assert_eq!(call["input"]["tag"], "open");
    }

    #[test]
    fn names_missing_arguments() {
        let schema = json!({ "properties": { "name": { "type": "string" } }, "required": ["name"] });
        let err = build_call("ts", "ts.t", &schema, &Map::new()).unwrap_err();
        assert!(err.contains("missing required argument(s): name"));
    }
}
