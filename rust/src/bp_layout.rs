//! blueprint(auto_layout) and blueprint(restore_layout).
//!
//! Agents spend tokens arranging nodes as they add them and still leave a
//! heap. The bp-graph-layout row layout (src/layout) makes that unnecessary:
//! an agent drops nodes anywhere, wires them, and asks for one layout pass.
//! Every run writes a report holding each node's old position, so it can be
//! put back exactly.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::dispatch::Hub;
use crate::layout;

const READ_PAGE: u64 = 2000;

fn flag(p: &Map<String, Value>, key: &str) -> bool {
    p.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn require_success(answer: &Value, method: &str) -> Result<(), String> {
    if answer.get("success") == Some(&Value::Bool(false)) {
        let why = answer.get("error").or_else(|| answer.get("message")).map(|v| v.to_string()).unwrap_or_else(|| answer.to_string());
        return Err(format!("{method}: {why}"));
    }
    Ok(())
}

fn first_line(s: &str) -> &str {
    s.split('\n').next().unwrap_or("")
}

/// ISO 8601 UTC for the report, without a date crate.
fn utc_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

impl Hub {
    pub async fn auto_layout(&self, owner: &str, p: &Map<String, Value>) -> Result<Value, String> {
        let asset = p.get("assetPath").or_else(|| p.get("path")).and_then(Value::as_str).ok_or("auto_layout needs assetPath")?.to_string();
        let dry_run = flag(p, "dryRun");
        let force = flag(p, "force");
        if !dry_run {
            self.refuse_if_locked_by_other(&asset, owner)?;
            self.refuse_during_pie(force).await?;
        }
        let graphs = self.layout_targets(owner, &asset, p).await?;
        let mut results = Vec::new();
        for graph in &graphs {
            results.push(self.layout_one(&asset, graph, dry_run, force).await?);
        }
        let saved = !dry_run && flag(p, "save");
        if saved {
            self.save_blueprint(&asset).await?;
        }
        if !dry_run {
            self.forget_touched(owner, &asset);
        }
        Ok(json!({
            "success": true,
            "assetPath": asset,
            "dryRun": dry_run,
            "saved": saved,
            "graphs": results,
            "note": if dry_run { "Nothing moved: dry run." } else { "Positions only: nothing was compiled. restore_layout with a report path puts a graph back." },
        }))
    }

    pub async fn restore_layout(&self, owner: &str, p: &Map<String, Value>) -> Result<Value, String> {
        let path = p.get("report").and_then(Value::as_str).ok_or("restore_layout needs report: the .layout.json path auto_layout returned")?;
        let path = {
            let given = PathBuf::from(path);
            if given.is_absolute() { given } else { self.layout_dir().join(given) }
        };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let report: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let asset = report["asset"].as_str().ok_or("report has no asset")?.to_string();
        let graph = report["graph"].as_str().ok_or("report has no graph")?.to_string();
        self.refuse_if_locked_by_other(&asset, owner)?;
        self.refuse_during_pie(flag(p, "force")).await?;
        let planned: Vec<(String, i64, i64)> = report["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| {
                let guid = n["guid"].as_str()?;
                Some((guid.to_string(), n["from"]["x"].as_f64()? as i64, n["from"]["y"].as_f64()? as i64))
            })
            .collect();
        let moved = self.move_nodes(&asset, &graph, &planned).await?;
        let saved = flag(p, "save");
        if saved {
            self.save_blueprint(&asset).await?;
        }
        Ok(json!({ "success": true, "assetPath": asset, "graph": graph, "restored": moved, "saved": saved }))
    }

    /// Graphs to lay out: named ones, every graph with nodes, the graphs this
    /// agent added nodes to, or EventGraph.
    async fn layout_targets(&self, owner: &str, asset: &str, p: &Map<String, Value>) -> Result<Vec<String>, String> {
        if let Some(g) = p.get("graphName").or_else(|| p.get("graph")).and_then(Value::as_str) {
            return Ok(vec![g.to_string()]);
        }
        if let Some(list) = p.get("graphs").and_then(Value::as_array) {
            return Ok(list.iter().filter_map(Value::as_str).map(str::to_string).collect());
        }
        if flag(p, "all") {
            let answer = self.bridge_call("list_blueprint_graphs", json!({ "assetPath": asset }), None).await?;
            require_success(&answer, "list_blueprint_graphs")?;
            return Ok(answer["graphs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|g| g["nodeCount"].as_u64().unwrap_or(0) > 1)
                .filter_map(|g| g["selector"].as_str().or_else(|| g["name"].as_str()).map(str::to_string))
                .collect());
        }
        let touched = self.touched_graphs(owner, asset);
        Ok(if touched.is_empty() { vec!["EventGraph".to_string()] } else { touched })
    }

    async fn layout_one(&self, asset: &str, graph: &str, dry_run: bool, force: bool) -> Result<Value, String> {
        let exported = self.bridge_call("export_nodes_t3d", json!({ "assetPath": asset, "graphName": graph }), Some(180_000)).await?;
        require_success(&exported, "export_nodes_t3d")?;
        let t3d = exported["t3d"].as_str().ok_or("export_nodes_t3d returned no t3d")?;
        let mut nodes = layout::parse_t3d(t3d);
        let info = self.read_graph_nodes(asset, graph, true).await?;
        layout::apply_graph_info(&mut nodes, &json!({ "nodes": info }));

        let foreign = layout::foreign_classes(&nodes);
        if !foreign.is_empty() && !force {
            return Ok(json!({
                "graph": graph,
                "skipped": format!("not a Blueprint (K2) graph ({}); pass force:true to lay it out anyway", foreign.iter().take(4).cloned().collect::<Vec<_>>().join(", ")),
            }));
        }

        let res = layout::layout_graph(&nodes);
        let (overlaps, backward) = layout::check(&nodes, &res);
        let title_of = |name: &str| -> String {
            nodes.iter().find(|n| n.name == name).and_then(|n| n.title.clone()).unwrap_or_default()
        };
        let report_nodes: Vec<Value> = nodes
            .iter()
            .map(|n| {
                let to = res.pos.get(&n.name).map(|(x, y)| json!({ "x": x, "y": y }));
                json!({
                    "name": n.name,
                    "guid": n.guid,
                    "title": first_line(n.title.as_deref().unwrap_or(&n.cls)),
                    "from": { "x": n.x as i64, "y": n.y as i64 },
                    "to": to,
                })
            })
            .collect();
        let rows: Vec<Value> = res
            .rows
            .iter()
            .map(|r| {
                let title = title_of(&r.root);
                json!({
                    "root": r.root,
                    "title": first_line(if title.is_empty() { &r.title } else { &title }),
                    "top": layout::js_round(r.top),
                    "bottom": layout::js_round(r.bottom),
                    "width": layout::js_round(r.right),
                })
            })
            .collect();
        let report = json!({
            "asset": asset,
            "graph": graph,
            "createdAt": utc_now(),
            "rows": rows,
            "warnings": res.warnings,
            "overlaps": overlaps,
            "backwardExecWires": backward,
            "nodes": report_nodes,
        });
        let report_path = self.write_layout_report(asset, graph, &report, &layout::preview_svg(&nodes, &res));

        let mut out = json!({
            "graph": graph,
            "nodes": nodes.len(),
            "rows": rows.iter().map(|r| r["title"].clone()).collect::<Vec<_>>(),
            "estimatedOverlaps": overlaps,
            "backwardExecWires": backward,
            "warnings": res.warnings,
            "commentsParkedBelowRows": res.parked.len(),
            "report": report_path,
        });
        if !dry_run {
            let planned: Vec<(String, i64, i64)> = nodes
                .iter()
                .filter_map(|n| {
                    let (x, y) = *res.pos.get(&n.name)?;
                    let guid = n.guid.clone()?;
                    (x != n.x as i64 || y != n.y as i64).then_some((guid, x, y))
                })
                .collect();
            out["moved"] = self.move_nodes(asset, graph, &planned).await?;
        }
        Ok(out)
    }

    /// Move nodes, skipping those already in place, then read every position
    /// back: a stale node name is answered with success by the plugin.
    async fn move_nodes(&self, asset: &str, graph: &str, planned: &[(String, i64, i64)]) -> Result<Value, String> {
        let before = positions_by_guid(&self.read_graph_nodes(asset, graph, false).await?);
        let moves: Vec<&(String, i64, i64)> = planned.iter().filter(|(g, x, y)| before.get(g) != Some(&(*x, *y))).collect();
        let batched = self.bridge.registered_methods().is_some_and(|m| m.contains("set_node_positions"));
        if !moves.is_empty() {
            if batched {
                let positions: Vec<Value> = moves.iter().map(|(g, x, y)| json!({ "nodeId": g, "posX": x, "posY": y })).collect();
                let answer = self
                    .bridge_call("set_node_positions", json!({ "assetPath": asset, "graphName": graph, "positions": positions }), Some(300_000))
                    .await?;
                require_success(&answer, "set_node_positions")?;
            } else {
                // Stock plugin: one call per node, and no undo.
                for (g, x, y) in &moves {
                    let answer = self
                        .bridge_call("set_node_position", json!({ "assetPath": asset, "graphName": graph, "nodeId": g, "posX": x, "posY": y }), None)
                        .await?;
                    require_success(&answer, "set_node_position")?;
                }
            }
        }
        let now = positions_by_guid(&self.read_graph_nodes(asset, graph, false).await?);
        let wrong: Vec<&String> = planned.iter().filter(|(g, x, y)| now.get(g) != Some(&(*x, *y))).map(|(g, _, _)| g).collect();
        if !wrong.is_empty() {
            return Err(format!("{} node(s) did not end where planned, e.g. {}", wrong.len(), wrong[0]));
        }
        Ok(json!({
            "moved": moves.len(),
            "alreadyInPlace": planned.len() - moves.len(),
            "verified": true,
            "undo": if batched { "one Ctrl+Z in the editor, or restore_layout" } else { "restore_layout (this plugin build moves nodes one by one, without a transaction)" },
        }))
    }

    async fn read_graph_nodes(&self, asset: &str, graph: &str, with_pins: bool) -> Result<Vec<Value>, String> {
        let mut nodes = Vec::new();
        let mut offset = 0u64;
        loop {
            let answer = self
                .bridge_call(
                    "read_blueprint_graph",
                    json!({
                        "assetPath": asset, "graphName": graph, "includePins": with_pins, "includeDefaults": with_pins,
                        "includeComments": true, "offset": offset, "limit": READ_PAGE,
                    }),
                    Some(120_000),
                )
                .await?;
            require_success(&answer, "read_blueprint_graph")?;
            nodes.extend(answer["nodes"].as_array().cloned().unwrap_or_default());
            if answer["hasMore"] != Value::Bool(true) {
                return Ok(nodes);
            }
            offset = answer["nextOffset"].as_u64().unwrap_or(offset + READ_PAGE);
        }
    }

    async fn refuse_during_pie(&self, force: bool) -> Result<(), String> {
        if force {
            return Ok(());
        }
        let answer = self.bridge_call("list_pie_instances", json!({}), None).await?;
        if answer["count"].as_u64().unwrap_or(0) > 0 {
            return Err("Play In Editor is running; end it first (moving nodes of a Blueprint with live PIE instances has crashed the editor).".into());
        }
        Ok(())
    }

    fn refuse_if_locked_by_other(&self, asset: &str, owner: &str) -> Result<(), String> {
        match self.locks.conflict(&[crate::locks::normalize(asset)], owner) {
            Some(c) => Err(format!("{asset} is locked by another agent: {c}")),
            None => Ok(()),
        }
    }

    async fn save_blueprint(&self, asset: &str) -> Result<(), String> {
        let package = asset.split('.').next().unwrap_or(asset);
        let answer = self.bridge_call("save_asset", json!({ "assetPath": package }), None).await?;
        require_success(&answer, "save_asset")
    }

    fn layout_dir(&self) -> PathBuf {
        match &self.project {
            Some(p) => p.root.join("Saved").join("BPLayout"),
            None => std::env::temp_dir().join("ue-mcp-rs").join("BPLayout"),
        }
    }

    /// `<Asset>.<Graph>.layout.json` plus an SVG sketch; None when it can't be written.
    fn write_layout_report(&self, asset: &str, graph: &str, report: &Value, svg: &str) -> Option<String> {
        let dir = self.layout_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let asset_name = asset.rsplit('/').next().unwrap_or(asset).split('.').next().unwrap_or(asset);
        let graph_name: String = graph.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
        let base = format!("{asset_name}.{graph_name}");
        let path = dir.join(format!("{base}.layout.json"));
        std::fs::write(&path, serde_json::to_string_pretty(report).ok()?).ok()?;
        let _ = std::fs::write(dir.join(format!("{base}.svg")), svg);
        Some(path.to_string_lossy().into_owned())
    }
}

fn positions_by_guid(nodes: &[Value]) -> IndexMap<String, (i64, i64)> {
    nodes
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?.to_string(), (n["posX"].as_f64()? as i64, n["posY"].as_f64()? as i64))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_format_is_iso() {
        let s = utc_now();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z') && s.as_bytes()[10] == b'T');
    }
}
