//! Parity tests: every fixture's expected output was produced by the original JS (bp-graph-layout skill)
//! from the same T3D + read_blueprint_graph input.
//!   <stem>.t3d / <stem>.graph.json   input captured from the editor (or edited from a capture)
//!   <stem>.expected.json             layoutGraph() answer: pos, size, rows (rounded), warnings, order
//!   <stem>.check.json                bp_layout.mjs overlap / backward-exec-wire counts
//!   <stem>.preview.svg               previewSvg() output

use std::path::PathBuf;

use serde_json::Value;

use super::*;

const FIXTURES: [&str; 3] = ["messy_eventgraph", "messy_plus", "widget_mix"];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bp_layout").join(name)
}

fn read(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn json(name: &str) -> Value {
    serde_json::from_str(&read(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn run(stem: &str) -> (Vec<Node>, LayoutResult) {
    let mut nodes = parse_t3d(&read(&format!("{stem}.t3d")));
    apply_graph_info(&mut nodes, &json(&format!("{stem}.graph.json")));
    let r = layout_graph(&nodes);
    (nodes, r)
}

/// JSON number (or null for NaN / undefined) vs a value of ours.
fn same(expected: &Value, actual: f64) -> bool {
    match expected.as_f64() {
        Some(e) => e == actual,
        None => expected.is_null() && actual.is_nan(),
    }
}

fn layout_mismatches(stem: &str) -> Vec<String> {
    let (_, r) = run(stem);
    let exp = json(&format!("{stem}.expected.json"));
    let mut bad = Vec::new();

    let epos = exp["pos"].as_object().expect("pos");
    let ekeys: Vec<&String> = epos.keys().collect();
    let akeys: Vec<&String> = r.pos.keys().collect();
    if ekeys != akeys {
        bad.push(format!("pos keys/order differ:\n  js:   {ekeys:?}\n  rust: {akeys:?}"));
    }
    for (k, v) in epos {
        // The JS parks comments at {x: cx, y} without rounding, so y can end in .5 there; pos is integral here,
        // and gets Math.round of the same number.
        let parked = r.parked.contains(k);
        let want = |e: &Value| if parked { e.as_f64().map(|f| Value::from(js_round(f))).unwrap_or(Value::Null) } else { e.clone() };
        match r.pos.get(k) {
            Some(&(x, y)) if same(&want(&v["x"]), x as f64) && same(&want(&v["y"]), y as f64) => {}
            got => bad.push(format!("pos {k}: js {v} rust {got:?}")),
        }
    }

    let esize = exp["size"].as_object().expect("size");
    if esize.keys().collect::<Vec<_>>() != r.size.keys().collect::<Vec<_>>() {
        bad.push("size keys/order differ".into());
    }
    for (k, v) in esize {
        match r.size.get(k) {
            Some(&(w, h)) if same(&v["w"], w) && same(&v["h"], h) => {}
            got => bad.push(format!("size {k}: js {v} rust {got:?}")),
        }
    }

    let erows = exp["rows"].as_array().expect("rows");
    if erows.len() != r.rows.len() {
        bad.push(format!("rows: js {} rust {}", erows.len(), r.rows.len()));
    }
    for (e, a) in erows.iter().zip(&r.rows) {
        let round = |v: f64| if v.is_nan() { f64::NAN } else { js_round(v) as f64 };
        if e["root"].as_str() != Some(a.root.as_str())
            || !same(&e["top"], round(a.top))
            || !same(&e["bottom"], round(a.bottom))
            || !same(&e["right"], round(a.right))
        {
            bad.push(format!("row: js {e} rust {a:?}"));
        }
    }

    let ewarn: Vec<&str> = exp["warnings"].as_array().expect("warnings").iter().filter_map(Value::as_str).collect();
    if ewarn != r.warnings.iter().map(String::as_str).collect::<Vec<_>>() {
        bad.push(format!("warnings: js {ewarn:?} rust {:?}", r.warnings));
    }
    let eorder: Vec<&str> = exp["order"].as_array().expect("order").iter().filter_map(Value::as_str).collect();
    if eorder != r.order.iter().map(String::as_str).collect::<Vec<_>>() {
        bad.push(format!("order: js {eorder:?} rust {:?}", r.order));
    }
    bad
}

#[test]
fn layout_parity_messy_eventgraph() {
    let bad = layout_mismatches("messy_eventgraph");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn layout_parity_messy_plus() {
    let bad = layout_mismatches("messy_plus");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn layout_parity_widget_mix() {
    let bad = layout_mismatches("widget_mix");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn layout_check_matches_js() {
    for stem in FIXTURES {
        let (nodes, r) = run(stem);
        let exp = json(&format!("{stem}.check.json"));
        let (overlaps, backward) = check(&nodes, &r);
        assert_eq!(overlaps as u64, exp["overlaps"].as_u64().unwrap(), "{stem} overlaps");
        assert_eq!(backward as u64, exp["backwardExecWires"].as_u64().unwrap(), "{stem} backward exec wires");
    }
    // The captured graph: no overlaps, and the one backward wire the JS reports too (OnHit's Set Counter feeds
    // the BeginPlay row's Print String, which BeginPlay owns because it comes first among equal path lengths).
    let (nodes, r) = run("messy_eventgraph");
    assert_eq!(check(&nodes, &r), (0, 1));
}

#[test]
fn layout_preview_matches_js() {
    for stem in FIXTURES {
        let (nodes, r) = run(stem);
        let svg = preview_svg(&nodes, &r);
        let exp = read(&format!("{stem}.preview.svg"));
        if svg != exp {
            let at = svg.bytes().zip(exp.bytes()).position(|(a, b)| a != b).unwrap_or(svg.len().min(exp.len()));
            let ctx = |s: &str| s.get(at.saturating_sub(80)..(at + 80).min(s.len())).unwrap_or("").to_string();
            panic!("{stem}: SVG differs at byte {at}\n  js:   {}\n  rust: {}", ctx(&exp), ctx(&svg));
        }
    }
}

#[test]
fn layout_parses_t3d_pins_and_links() {
    let nodes = parse_t3d(&read("messy_eventgraph.t3d"));
    assert_eq!(nodes.len(), 19);
    let seq = nodes.iter().find(|n| n.name == "K2Node_ExecutionSequence_0").unwrap();
    assert_eq!(seq.cls, "K2Node_ExecutionSequence");
    assert_eq!(seq.guid.as_deref(), Some("0698567D48ADD0D4172ABDA76F37EC10"));
    let then1 = seq.pins.iter().find(|p| p.name == "then_1").unwrap();
    assert_eq!(then1.dir, PinDir::Out);
    assert_eq!(then1.cat, "exec");
    assert_eq!(then1.links, vec![Link { node: "K2Node_CallFunction_3".into(), pin: "ECF47A3A42FE2CEDA0DC7B9DA78FEE0C".into() }]);
    let delay = nodes.iter().find(|n| n.name == "K2Node_CallFunction_3").unwrap();
    let latent = delay.pins.iter().find(|p| p.name == "LatentInfo").unwrap();
    assert_eq!(latent.def.as_deref(), Some(r#"(Linkage=-1,UUID=-1,ExecutionFunction="",CallbackTarget=None)"#));
    assert!(latent.hidden);
    let completed = delay.pins.iter().find(|p| p.name == "then").unwrap();
    assert_eq!(completed.friendly.as_deref(), Some(r#"NSLOCTEXT("", "015C33674C8C2145B703DDB2CB3BE554", "Completed")"#));
    let tick = nodes.iter().find(|n| n.name == "K2Node_Event_2").unwrap();
    assert_eq!((tick.x, tick.y), (0.0, 416.0));
    assert_eq!(foreign_classes(&nodes), Vec::<String>::new());
}

#[test]
fn layout_graph_info_and_foreign_classes() {
    let mut nodes = parse_t3d(
        "Begin Object Class=/Script/BlueprintGraph.K2Node_CallFunction Name=\"K2Node_CallFunction_0\"\n\
         NodeGuid=AA\n\
         CustomProperties Pin (PinId=P1,PinName=\"InText\",PinType.PinCategory=\"text\",)\n\
         CustomProperties Pin (PinId=P2,PinName=\"InText\",Direction=\"EGPD_Output\",PinType.PinCategory=\"text\",)\n\
         End Object\n\
         Begin Object Class=/Script/AnimGraph.AnimGraphNode_Root Name=\"AnimGraphNode_Root_0\"\n\
         End Object\n\
         Begin Object Class=/Script/UnrealEd.EdGraphNode_Comment Name=\"EdGraphNode_Comment_0\"\n\
         End Object\n\
         Begin Object Class=/Script/AnimGraph.AnimGraphNode_Root Name=\"AnimGraphNode_Root_1\"\n\
         End Object\n",
    );
    apply_graph_info(
        &mut nodes,
        &serde_json::json!({ "nodes": [{ "id": "AA", "title": "Set Text\nTarget is Text Block", "pins": [
            { "name": "InText", "direction": "Output", "defaultTextValue": "out" },
            { "name": "InText", "direction": "Input", "defaultTextValue": "in" },
        ]}]}),
    );
    assert_eq!(nodes[0].title.as_deref(), Some("Set Text\nTarget is Text Block"));
    assert_eq!(nodes[0].pins[0].text.as_deref(), Some("in"));
    assert_eq!(nodes[0].pins[1].text.as_deref(), Some("out"));
    assert_eq!(foreign_classes(&nodes), vec!["AnimGraphNode_Root".to_string()]);
}

/// Degenerate graphs, against answers the JS gave for the same text.
#[test]
fn layout_empty_and_comment_only_graphs() {
    const EMPTY_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-40 -40 -Infinity -Infinity" width="-Infinity" height="-Infinity"><rect x="-40" y="-40" width="-Infinity" height="-Infinity" fill="#151515"/></svg>"##;
    let r = layout_graph(&[]);
    assert!(r.pos.is_empty() && r.rows.is_empty() && r.warnings.is_empty());
    assert_eq!(preview_svg(&[], &r), EMPTY_SVG);

    let nodes = parse_t3d(
        "Begin Object Class=/Script/UnrealEd.EdGraphNode_Comment Name=\"EdGraphNode_Comment_3\"\n   NodeWidth=250.5\nEnd Object\n\
         Begin Object Class=/Script/UnrealEd.EdGraphNode_Comment Name=\"C2\"\n   NodeWidth=abc\nEnd Object\n",
    );
    let r = layout_graph(&nodes);
    // JS: {"EdGraphNode_Comment_3":{"x":0,"y":0},"C2":{"x":330.5,"y":0}}; the parked x is Math.round-ed here
    assert_eq!(r.pos.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), vec![("EdGraphNode_Comment_3", (0, 0)), ("C2", (331, 0))]);
    assert_eq!(r.parked, vec!["EdGraphNode_Comment_3", "C2"]);
    assert_eq!(r.warnings, vec!["2 comment box(es) were moved below the rows; re-wrap them by hand"]);
    assert_eq!(preview_svg(&nodes, &r), EMPTY_SVG);
    assert_eq!(check(&nodes, &r), (0, 0));
}
