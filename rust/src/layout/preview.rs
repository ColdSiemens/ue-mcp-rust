//! SVG sketch of a layout: estimated node boxes, titles and wires. Port of `scripts/lib/preview.mjs`.

use std::collections::HashMap;

use super::engine::LayoutResult;
use super::geom::title_or_cls;
use super::js::{self, num_str as n};
use super::t3d::{Node, PinDir};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

fn wire(cat: &str) -> &'static str {
    match cat {
        "exec" => "#fff",
        "bool" => "#8c0000",
        "int" => "#1fe3af",
        "real" => "#9cfc4c",
        "name" => "#c97bff",
        "string" => "#ff00d4",
        "text" => "#e87ab2",
        "object" => "#00a8f5",
        "struct" => "#0059cb",
        "byte" => "#006f65",
        "class" => "#5800a8",
        "delegate" => "#ff3838",
        _ => "#aaa",
    }
}

/// An SVG of the laid-out graph (comments are not drawn). Open it in a browser to eyeball a dry run.
pub fn preview_svg(nodes: &[Node], r: &LayoutResult) -> String {
    let placed: Vec<&Node> = nodes.iter().filter(|n| r.pos.contains_key(&n.name) && r.geom.contains_key(&n.name)).collect();
    let by: HashMap<&str, &Node> = placed.iter().map(|n| (n.name.as_str(), *n)).collect();
    let bx = |nd: &Node| {
        let (x, y) = r.pos[&nd.name];
        let g = &r.geom[&nd.name];
        (x as f64, y as f64, g.w, g.h)
    };
    let w = js::max_of(f64::NEG_INFINITY, placed.iter().map(|nd| bx(nd).0 + bx(nd).2)) + 40.0;
    let h = js::max_of(f64::NEG_INFINITY, placed.iter().map(|nd| bx(nd).1 + bx(nd).3)) + 40.0;
    let mut wires = String::new();
    let mut boxes = String::new();
    for nd in &placed {
        for p in &nd.pins {
            if p.dir != PinDir::Out {
                continue;
            }
            for l in &p.links {
                let Some(t) = by.get(l.node.as_str()) else { continue };
                let (ax, ay, aw, ah) = bx(nd);
                let (bx_, by_, _, bh) = bx(t);
                let x1 = ax + aw;
                let y1 = ay + r.geom[&nd.name].pin_y.get(&p.id).copied().unwrap_or(ah / 2.0);
                let x2 = bx_;
                let y2 = by_ + r.geom[&t.name].pin_y.get(&l.pin).copied().unwrap_or(bh / 2.0);
                let dx = js::max(60.0, (x2 - x1).abs() / 2.0);
                wires += &format!(
                    r#"<path d="M{},{} C{},{} {},{} {},{}" stroke="{}" stroke-width="{}" fill="none" opacity=".8"/>"#,
                    n(x1),
                    n(y1),
                    n(x1 + dx),
                    n(y1),
                    n(x2 - dx),
                    n(y2),
                    n(x2),
                    n(y2),
                    wire(&p.cat),
                    if p.cat == "exec" { 3 } else { 2 }
                );
            }
        }
    }
    for nd in &placed {
        let (x, y, bw, bh) = bx(nd);
        let exec = nd.pins.iter().any(|p| p.cat == "exec" && !p.hidden);
        let mut lines = title_or_cls(nd).split('\n');
        let t1 = lines.next().unwrap_or("");
        let t2 = lines.next().unwrap_or("");
        let head = if nd.cls.contains("Event") {
            "#7a1010"
        } else if exec {
            "#2d5a9e"
        } else {
            "#3f7a3f"
        };
        boxes += &format!(
            r##"<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="6" fill="#1d2329" stroke="#000" stroke-width="2"/>"##,
            x = n(x),
            y = n(y),
            w = n(bw),
            h = n(bh)
        );
        boxes += &format!(
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="6" fill="{}"/>"#,
            n(x),
            n(y),
            n(bw),
            n(js::min(26.0, bh)),
            head
        );
        boxes += &format!(
            r##"<text x="{}" y="{}" fill="#fff" font-family="Segoe UI,Arial" font-size="13" font-weight="bold">{}</text>"##,
            n(x + 8.0),
            n(y + 18.0),
            esc(t1)
        );
        if !t2.is_empty() {
            boxes += &format!(
                r##"<text x="{}" y="{}" fill="#bbb" font-family="Segoe UI,Arial" font-size="11">{}</text>"##,
                n(x + 8.0),
                n(y + 36.0),
                esc(t2)
            );
        }
    }
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-40 -40 {vw} {vh}" width="{vw}" height="{vh}"><rect x="-40" y="-40" width="{rw}" height="{rh}" fill="#151515"/>{wires}{boxes}</svg>"##,
        vw = n(w + 40.0),
        vh = n(h + 40.0),
        rw = n(w + 80.0),
        rh = n(h + 80.0)
    )
}
