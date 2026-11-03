//! Row layout for one Blueprint (K2) graph. Port of `scripts/lib/layout.mjs`.
//! - Every event / function entry starts its own row; rows are stacked top to bottom with a wide gap.
//! - Exec nodes run left to right on a spine with their exec pins on one line, so exec wires are straight.
//! - The pure nodes feeding a node sit left of it, under the exec wire, as a right-to-left tree.
//! - Extra exec outputs (Branch False, Cast Failed, loop Completed, Sequence Then 1...) drop to sub-rows below,
//!   the rightmost node's first, so a branch wire never has to cross another sub-row.
//! - A node with several exec sources waits until every source is placed and then goes right of the last one,
//!   so merge wires always run forward. A node reachable from several events joins the row with the longest
//!   path to it, and the other events' rows are moved directly above that row.

use std::collections::{HashMap, HashSet, VecDeque};

use indexmap::{IndexMap, IndexSet};

use super::geom::{geom, member_name, Geom};
use super::js;
use super::t3d::{Node, Pin, PinDir};

/// Spacing, in graph units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Opt {
    /// exec node -> next exec node, or -> the left edge of the next node's input block
    pub exec_gap: f64,
    /// input block -> its exec consumer
    pub block_gap: f64,
    /// between pure nodes of one block
    pub pure_gap_x: f64,
    /// between stacked pure subtrees
    pub pure_gap_y: f64,
    /// input blocks start this far below the exec pin line
    pub under_wire: f64,
    /// clearance kept when a sub-row drops below placed nodes
    pub coll_x: f64,
    pub coll_y: f64,
    /// between event rows
    pub row_gap: f64,
}

pub const OPT: Opt = Opt {
    exec_gap: 70.0,
    block_gap: 50.0,
    pure_gap_x: 40.0,
    pure_gap_y: 20.0,
    under_wire: 20.0,
    coll_x: 30.0,
    coll_y: 44.0,
    row_gap: 260.0,
};

const ENGINE_EVENTS: [&str; 8] = ["OnInitialized", "PreConstruct", "Construct", "Destruct", "BeginPlay", "EndPlay", "Tick", "OnPaint"];
const LEFTOVER: &str = "__leftover";
const LOOSE: &str = "__loose";

/// One stacked row. `top`/`bottom`/`right` are unrounded graph units (round them with `js_round`);
/// they are NaN for a row that ended up with no nodes (the JS leaves them undefined).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Root node name, or "__leftover" / "__loose".
    pub root: String,
    /// The root's title ("" when it has none), or a description for the two synthetic rows.
    pub title: String,
    pub top: f64,
    pub bottom: f64,
    pub right: f64,
}

#[derive(Debug, Clone)]
pub struct LayoutResult {
    /// New top-left of every node, comments included, in the order the JS object enumerates.
    pub pos: IndexMap<String, (i64, i64)>,
    /// Estimated (w, h) of every non-comment node.
    pub size: IndexMap<String, (f64, f64)>,
    /// Estimated geometry (size, exec line, pin offsets) of every non-comment node; used by the preview.
    pub geom: IndexMap<String, Geom>,
    pub rows: Vec<Row>,
    /// Comment boxes, parked below the rows.
    pub parked: Vec<String>,
    pub warnings: Vec<String>,
    /// Root names in row order.
    #[allow(dead_code)]
    pub order: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

struct BlockItem<'a> {
    name: &'a str,
    rx: f64,
    ry: f64,
}

struct ChainItem<'a> {
    n: &'a Node,
    nx: f64,
    blk: Vec<BlockItem<'a>>,
}

fn ex(p: &Pin) -> bool {
    p.cat == "exec" && !p.hidden
}

fn is_exec(n: &Node) -> bool {
    n.pins.iter().any(ex)
}

fn exec_outs(n: &Node) -> impl Iterator<Item = &Pin> {
    n.pins.iter().filter(|p| ex(p) && p.dir == PinDir::Out)
}

/// `a - b || c - d || ...` with JS truthiness (0 and NaN fall through).
fn or_else(d: f64, next: impl FnOnce() -> f64) -> f64 {
    if d != 0.0 && !d.is_nan() {
        d
    } else {
        next()
    }
}

struct Ctx<'a> {
    opt: Opt,
    by: HashMap<&'a str, &'a Node>,
    g: HashMap<&'a str, Geom>,
    preds_of: HashMap<&'a str, IndexSet<&'a str>>,
    owner: IndexMap<&'a str, Option<&'a str>>,
    pos: IndexMap<&'a str, (f64, f64)>,
    node_row: HashMap<&'a str, &'a str>,
    spine_of: HashMap<&'a str, f64>,
    exec_placed: HashSet<&'a str>,
    claimed: HashSet<&'a str>,
    rects: Vec<Rect>,
    cur: &'a str,
    deferred: IndexSet<&'a str>,
}

impl<'a> Ctx<'a> {
    fn geom_of(&self, name: &str) -> &Geom {
        &self.g[name]
    }

    /// `owner[name] === cur`
    fn owned(&self, name: &str) -> bool {
        matches!(self.owner.get(name), Some(Some(o)) if *o == self.cur)
    }

    fn own_preds(&self, name: &str) -> Vec<&'a str> {
        self.preds_of.get(name).map_or_else(Vec::new, |s| s.iter().copied().filter(|p| self.owned(p)).collect())
    }

    fn is_merge(&self, name: &str) -> bool {
        self.own_preds(name).len() >= 2
    }

    fn ready(&self, name: &str) -> bool {
        self.own_preds(name).iter().all(|p| self.exec_placed.contains(p))
    }

    fn out_pin_y(&self, src: &str, pin_id: &str) -> f64 {
        let g = self.geom_of(src);
        g.pin_y.get(pin_id).copied().unwrap_or(g.h / 2.0)
    }

    fn place_pure(&mut self, p: &'a Node, right_x: f64, top: f64, items: &mut Vec<BlockItem<'a>>) -> f64 {
        self.claimed.insert(&p.name);
        let (gw, gh) = (self.geom_of(&p.name).w, self.geom_of(&p.name).h);
        let rx = right_x - gw;
        items.push(BlockItem { name: &p.name, rx, ry: top });
        let mut cy = top;
        let mut bottom = top + gh;
        for q in &p.pins {
            if q.dir != PinDir::In || q.cat == "exec" {
                continue;
            }
            for l in &q.links {
                let Some(&src) = self.by.get(l.node.as_str()) else { continue };
                if is_exec(src) || self.claimed.contains(src.name.as_str()) {
                    continue;
                }
                let qy = self.geom_of(&p.name).pin_y.get(&q.id).copied().unwrap_or(gh / 2.0);
                let t = js::max(cy, top + qy - self.out_pin_y(&src.name, &l.pin));
                let b = self.place_pure(src, rx - self.opt.pure_gap_x, t, items);
                cy = b + self.opt.pure_gap_y;
                bottom = js::max(bottom, b);
            }
        }
        bottom
    }

    fn build_block(&mut self, n: &'a Node) -> (Vec<BlockItem<'a>>, f64) {
        let (exec_y, gh) = (self.geom_of(&n.name).exec_y, self.geom_of(&n.name).h);
        let mut items = Vec::new();
        let mut cursor = exec_y + self.opt.under_wire;
        for p in &n.pins {
            if p.dir != PinDir::In || p.cat == "exec" {
                continue;
            }
            for l in &p.links {
                let Some(&src) = self.by.get(l.node.as_str()) else { continue };
                if is_exec(src) || self.claimed.contains(src.name.as_str()) {
                    continue;
                }
                let py = self.geom_of(&n.name).pin_y.get(&p.id).copied().unwrap_or(gh / 2.0);
                let top = js::max(cursor, py - self.out_pin_y(&src.name, &l.pin));
                cursor = self.place_pure(src, -self.opt.block_gap, top, &mut items) + self.opt.pure_gap_y;
            }
        }
        let min_rx = js::min_of(0.0, items.iter().map(|i| i.rx));
        (items, min_rx)
    }

    /// Lowest spine y >= min_s where none of `rel` (spine-relative rects) hits a placed rect of this row.
    fn find_spine(&self, rel: &[Rect], min_s: f64) -> f64 {
        let o = &self.opt;
        let mut s = min_s;
        for _ in 0..4000 {
            let mut need: Option<f64> = None;
            for r in rel {
                for e in &self.rects {
                    if r.x0 < e.x1 + o.coll_x
                        && r.x1 + o.coll_x > e.x0
                        && r.y0 + s < e.y1 + o.coll_y
                        && r.y1 + s + o.coll_y > e.y0
                    {
                        let v = e.y1 + o.coll_y - r.y0;
                        if need.is_none_or(|nd| v > nd) {
                            need = Some(v);
                        }
                    }
                }
            }
            match need {
                None => return s,
                Some(nd) => s = js::max(s + 1.0, nd),
            }
        }
        s
    }

    fn layout_chain(&mut self, start: &'a Node, x0: f64, min_s: f64, via_defer: bool) {
        let mut chain: Vec<&'a Node> = Vec::new();
        let mut first = true;
        let mut cursor = Some(start);
        while let Some(n) = cursor {
            if self.exec_placed.contains(n.name.as_str()) || !self.owned(&n.name) {
                break;
            }
            // `n === start` only holds on the first step: start is exec-placed after it
            if self.is_merge(&n.name) && !(via_defer && first) {
                self.deferred.insert(&n.name);
                break;
            }
            first = false;
            self.exec_placed.insert(&n.name);
            chain.push(n);
            let mut next = None;
            'outs: for p in exec_outs(n) {
                for l in &p.links {
                    let Some(&t) = self.by.get(l.node.as_str()) else { continue };
                    if !self.exec_placed.contains(t.name.as_str()) && self.owned(&t.name) && !self.is_merge(&t.name) {
                        next = Some(t);
                        break 'outs;
                    }
                }
            }
            cursor = next;
        }
        if chain.is_empty() {
            return;
        }
        let mut x = x0;
        let mut items: Vec<ChainItem<'a>> = Vec::new();
        for c in chain {
            let (blk, min_rx) = self.build_block(c);
            let nx = x + if !blk.is_empty() { -min_rx } else { 0.0 };
            x = nx + self.geom_of(&c.name).w + self.opt.exec_gap;
            items.push(ChainItem { n: c, nx, blk });
        }
        let mut rel = Vec::new();
        for it in &items {
            let g = self.geom_of(&it.n.name);
            let ny = -g.exec_y;
            rel.push(Rect { x0: it.nx, y0: ny, x1: it.nx + g.w, y1: ny + g.h });
            for b in &it.blk {
                let gb = self.geom_of(b.name);
                rel.push(Rect { x0: it.nx + b.rx, y0: ny + b.ry, x1: it.nx + b.rx + gb.w, y1: ny + b.ry + gb.h });
            }
        }
        let s = self.find_spine(&rel, min_s);
        for it in &items {
            let ny = s - self.geom_of(&it.n.name).exec_y;
            self.pos.insert(&it.n.name, (it.nx, ny));
            self.node_row.insert(&it.n.name, self.cur);
            self.spine_of.insert(&it.n.name, s);
            for b in &it.blk {
                self.pos.insert(b.name, (it.nx + b.rx, ny + b.ry));
                self.node_row.insert(b.name, self.cur);
            }
        }
        for r in &rel {
            self.rects.push(Rect { x0: r.x0, x1: r.x1, y0: r.y0 + s, y1: r.y1 + s });
        }
        for k in (0..items.len()).rev() {
            let it = &items[k];
            let main_next = items.get(k + 1).map(|i| i.n.name.as_str());
            for p in exec_outs(it.n) {
                for l in &p.links {
                    let Some(&t) = self.by.get(l.node.as_str()) else { continue };
                    if Some(t.name.as_str()) == main_next || self.exec_placed.contains(t.name.as_str()) || !self.owned(&t.name) {
                        continue;
                    }
                    if self.is_merge(&t.name) {
                        self.deferred.insert(&t.name);
                        continue;
                    }
                    let x0 = it.nx + self.geom_of(&it.n.name).w + self.opt.exec_gap;
                    self.layout_chain(t, x0, s, false);
                }
            }
        }
    }

    fn drain_deferred(&mut self) {
        let mut guard = 0;
        while !self.deferred.is_empty() && guard < 1000 {
            let mut list: Vec<&'a str> = self.deferred.iter().copied().filter(|t| !self.exec_placed.contains(t)).collect();
            if list.is_empty() {
                break;
            }
            let mut rd: Vec<&'a str> = list.iter().copied().filter(|t| self.ready(t)).collect();
            if rd.is_empty() {
                let placed_count = |t: &str| self.own_preds(t).iter().filter(|p| self.exec_placed.contains(*p)).count() as f64;
                js::sort_by(&mut list, &mut |a, b| placed_count(b) - placed_count(a));
                rd = vec![list[0]];
            }
            // two-way joins before wide sinks
            js::sort_by(&mut rd, &mut |a, b| self.own_preds(a).len() as f64 - self.own_preds(b).len() as f64);
            let t = rd[0];
            self.deferred.shift_remove(t);
            let placed: Vec<&'a str> = self.own_preds(t).into_iter().filter(|p| self.exec_placed.contains(p)).collect();
            let x0 = js::max_of(
                0.0,
                placed.iter().map(|p| self.pos.get(p).map_or(f64::NAN, |v| v.0) + self.geom_of(p).w),
            ) + self.opt.exec_gap;
            let min_s = if placed.is_empty() {
                0.0
            } else {
                js::min_of(f64::INFINITY, placed.iter().map(|p| self.spine_of.get(p).copied().unwrap_or(f64::NAN)))
            };
            let start = self.by[t];
            self.layout_chain(start, x0, min_s, true);
            guard += 1;
        }
    }
}

struct RowAcc<'a> {
    root: &'a str,
    title: String,
    rects: Vec<Rect>,
    shift: Option<f64>,
    top: f64,
    bottom: f64,
    right: f64,
}

/// Lay out a graph with the default spacing.
pub fn layout_graph(nodes: &[Node]) -> LayoutResult {
    layout_graph_with(nodes, &OPT)
}

pub fn layout_graph_with(all_nodes: &[Node], opt: &Opt) -> LayoutResult {
    let comments: Vec<&Node> = all_nodes.iter().filter(|n| n.cls == "EdGraphNode_Comment").collect();
    let nodes: Vec<&Node> = all_nodes.iter().filter(|n| n.cls != "EdGraphNode_Comment").collect();
    let mut by: HashMap<&str, &Node> = HashMap::new();
    for n in &nodes {
        by.insert(&n.name, n);
    }
    let g: HashMap<&str, Geom> = by.iter().map(|(k, n)| (*k, geom(n))).collect();
    let exec_nodes: Vec<&Node> = nodes.iter().copied().filter(|n| is_exec(n)).collect();
    let mut preds_of: HashMap<&str, IndexSet<&str>> = HashMap::new();
    for n in &exec_nodes {
        preds_of.insert(&n.name, IndexSet::new());
    }
    for n in &exec_nodes {
        for p in exec_outs(n) {
            for l in &p.links {
                if l.node != n.name {
                    if let Some(set) = preds_of.get_mut(l.node.as_str()) {
                        set.insert(&n.name);
                    }
                }
            }
        }
    }
    let roots: Vec<&Node> = exec_nodes
        .iter()
        .copied()
        .filter(|n| !n.pins.iter().any(|p| ex(p) && p.dir == PinDir::In && !p.links.is_empty()))
        .collect();

    // ---- row order: engine events, function entry, custom events, other events, loose chains
    let ev_name = |n: &Node| member_name(n.props.get("EventReference").map_or("", String::as_str)).unwrap_or("").to_string();
    let rank = |n: &Node| -> f64 {
        if n.cls == "K2Node_Event" {
            let e = ev_name(n);
            return ENGINE_EVENTS.iter().position(|x| *x == e).map_or(50.0, |i| i as f64);
        }
        if n.cls == "K2Node_FunctionEntry" || n.cls == "K2Node_Tunnel" {
            return 60.0;
        }
        if n.cls == "K2Node_CustomEvent" {
            return 100.0;
        }
        if n.cls.contains("Event") || n.cls.contains("Input") {
            return 200.0;
        }
        300.0
    };
    let mut order: Vec<&Node> = roots.clone();
    js::sort_by(&mut order, &mut |a, b| or_else(rank(a) - rank(b), || or_else(a.y - b.y, || a.x - b.x)));

    // ---- ownership of exec nodes several events reach: longest path wins
    let mut depth: HashMap<&str, HashMap<&str, i64>> = HashMap::new();
    for r in &order {
        let mut d: HashMap<&str, i64> = HashMap::from([(r.name.as_str(), 0)]);
        let mut q: VecDeque<&str> = VecDeque::from([r.name.as_str()]);
        let mut bumps: HashMap<&str, i64> = HashMap::new();
        while let Some(u) = q.pop_front() {
            // a link to a node outside the export would crash the JS here; skip it
            let Some(&un) = by.get(u) else { continue };
            for p in exec_outs(un) {
                for l in &p.links {
                    let nd = d[u] + 1;
                    if d.get(l.node.as_str()).copied().unwrap_or(-1) < nd {
                        let b = bumps.entry(l.node.as_str()).or_insert(0);
                        *b += 1;
                        if *b < 40 {
                            d.insert(&l.node, nd);
                            q.push_back(&l.node);
                        }
                    }
                }
            }
        }
        depth.insert(&r.name, d);
    }
    let mut owner: IndexMap<&str, Option<&str>> = IndexMap::new();
    for n in &exec_nodes {
        let mut best = None;
        let mut bd = -1;
        for r in &order {
            if let Some(&v) = depth[r.name.as_str()].get(n.name.as_str()) {
                if v > bd {
                    bd = v;
                    best = Some(r.name.as_str());
                }
            }
        }
        owner.insert(&n.name, best);
    }
    let owner_keys = js::object_key_order(owner.keys().copied());
    let truthy_owner = |name: &str| owner.get(name).copied().flatten().filter(|o| !o.is_empty());
    for r in order.clone() {
        let mut targets: IndexSet<&str> = IndexSet::new();
        for u in &owner_keys {
            if owner[*u] != Some(r.name.as_str()) {
                continue;
            }
            for p in exec_outs(by[*u]) {
                for l in &p.links {
                    if let Some(o) = truthy_owner(&l.node) {
                        if o != r.name {
                            targets.insert(o);
                        }
                    }
                }
            }
        }
        for t in targets {
            let ri = order.iter().position(|x| x.name == r.name);
            let ti = order.iter().position(|x| x.name == t);
            if let (Some(ri), Some(ti)) = (ri, ti) {
                if ti > ri + 1 {
                    let row = order.remove(ti);
                    order.insert(ri + 1, row);
                }
            }
        }
    }

    let mut cx = Ctx {
        opt: *opt,
        by,
        g,
        preds_of,
        owner,
        pos: IndexMap::new(),
        node_row: HashMap::new(),
        spine_of: HashMap::new(),
        exec_placed: HashSet::new(),
        claimed: HashSet::new(),
        rects: Vec::new(),
        cur: "",
        deferred: IndexSet::new(),
    };
    let mut rows: Vec<RowAcc> = Vec::new();
    let new_row = |root, title: String, rects| RowAcc { root, title, rects, shift: None, top: f64::NAN, bottom: f64::NAN, right: f64::NAN };
    for r in &order {
        cx.cur = r.name.as_str();
        cx.rects = Vec::new();
        cx.deferred = IndexSet::new();
        cx.layout_chain(r, 0.0, 0.0, false);
        cx.drain_deferred();
        rows.push(new_row(r.name.as_str(), r.title.clone().unwrap_or_default(), std::mem::take(&mut cx.rects)));
    }
    let mut warnings = Vec::new();
    let leftovers: Vec<&Node> = exec_nodes.iter().copied().filter(|n| !cx.exec_placed.contains(n.name.as_str())).collect();
    if !leftovers.is_empty() {
        cx.cur = LEFTOVER;
        cx.rects = Vec::new();
        cx.deferred = IndexSet::new();
        for n in &leftovers {
            cx.owner.insert(&n.name, Some(LEFTOVER));
        }
        for n in &leftovers {
            if !cx.exec_placed.contains(n.name.as_str()) {
                let min_s = if cx.rects.is_empty() {
                    0.0
                } else {
                    js::max_of(f64::NEG_INFINITY, cx.rects.iter().map(|r| r.y1)) + opt.row_gap / 2.0
                };
                cx.layout_chain(n, 0.0, min_s, true);
                cx.drain_deferred();
            }
        }
        rows.push(new_row(LEFTOVER, "exec nodes no event reaches".into(), std::mem::take(&mut cx.rects)));
        warnings.push(format!("{} exec node(s) no event reaches were put in a row of their own", leftovers.len()));
    }
    let loose: Vec<&Node> = nodes.iter().copied().filter(|n| !is_exec(n) && !cx.claimed.contains(n.name.as_str())).collect();
    if !loose.is_empty() {
        cx.rects = Vec::new();
        cx.cur = LOOSE;
        let mut x = 0.0;
        for n in &loose {
            let (w, h) = (cx.g[n.name.as_str()].w, cx.g[n.name.as_str()].h);
            cx.pos.insert(&n.name, (x, 0.0));
            cx.node_row.insert(&n.name, LOOSE);
            cx.rects.push(Rect { x0: x, x1: x + w, y0: 0.0, y1: h });
            x += w + opt.pure_gap_x;
        }
        rows.push(new_row(LOOSE, "pure nodes that feed nothing".into(), std::mem::take(&mut cx.rects)));
        let names: Vec<&str> = loose
            .iter()
            .map(|n| n.title.as_deref().filter(|t| !t.is_empty()).unwrap_or(&n.name).split('\n').next().unwrap_or(""))
            .collect();
        warnings.push(format!(
            "{} unconnected pure node(s) were lined up in the last row: {}",
            loose.len(),
            names.join(", ")
        ));
    }

    // stack the rows; each was laid out in its own coordinates
    let mut y = 0.0;
    for row in rows.iter_mut() {
        if row.rects.is_empty() {
            continue;
        }
        let top = js::min_of(f64::INFINITY, row.rects.iter().map(|r| r.y0));
        let bottom = js::max_of(f64::NEG_INFINITY, row.rects.iter().map(|r| r.y1));
        row.shift = Some(y - top);
        row.top = y;
        row.bottom = y + bottom - top;
        row.right = js::max_of(f64::NEG_INFINITY, row.rects.iter().map(|r| r.x1));
        y = row.bottom + opt.row_gap;
    }
    // `r.shift || 0`
    let shift: HashMap<&str, f64> = rows
        .iter()
        .map(|r| (r.root, r.shift.filter(|s| *s != 0.0 && !s.is_nan()).unwrap_or(0.0)))
        .collect();
    let mut out: IndexMap<String, (i64, i64)> = IndexMap::new();
    for name in js::object_key_order(cx.pos.keys().copied()) {
        let (px, py) = cx.pos[name];
        let sh = cx.node_row.get(name).and_then(|r| shift.get(r)).copied().unwrap_or(f64::NAN);
        out.insert(name.to_string(), (js::round_i(px), js::round_i(py + sh)));
    }

    // comment boxes cannot be resized through the bridge, so they wait below the rows for a human
    let mut parked = Vec::new();
    let mut cxp = 0.0;
    for c in &comments {
        // the JS stores {x: cx, y} unrounded; y can be fractional there
        out.insert(c.name.clone(), (js::round_i(cxp), js::round_i(y)));
        parked.push(c.name.clone());
        let w = c.props.get("NodeWidth").map_or(f64::NAN, |v| js::number(v));
        cxp += (if w != 0.0 && !w.is_nan() { w } else { 400.0 }) + 80.0;
    }
    if !parked.is_empty() {
        warnings.push(format!("{} comment box(es) were moved below the rows; re-wrap them by hand", parked.len()));
    }

    let mut size = IndexMap::new();
    let mut geoms = IndexMap::new();
    for n in &nodes {
        let gn = &cx.g[n.name.as_str()];
        size.insert(n.name.clone(), (gn.w, gn.h));
        geoms.insert(n.name.clone(), gn.clone());
    }
    LayoutResult {
        pos: js::object_order(out),
        size: js::object_order(size),
        geom: js::object_order(geoms),
        rows: rows
            .into_iter()
            .map(|r| Row { root: r.root.to_string(), title: r.title, top: r.top, bottom: r.bottom, right: r.right })
            .collect(),
        parked,
        warnings,
        order: order.iter().map(|r| r.name.clone()).collect(),
    }
}

/// Sanity checks on the estimated boxes, as bp_layout.mjs computes them: (estimated overlaps, backward exec wires).
pub fn check(nodes: &[Node], r: &LayoutResult) -> (usize, usize) {
    let placed: Vec<&Node> = nodes.iter().filter(|n| r.pos.contains_key(&n.name) && r.size.contains_key(&n.name)).collect();
    let bx = |n: &Node| {
        let (x, y) = r.pos[&n.name];
        let (w, h) = r.size[&n.name];
        (x as f64, y as f64, w, h)
    };
    let mut overlaps = 0;
    for i in 0..placed.len() {
        for j in i + 1..placed.len() {
            let (ax, ay, aw, ah) = bx(placed[i]);
            let (bx_, by_, bw, bh) = bx(placed[j]);
            if ax < bx_ + bw && bx_ < ax + aw && ay < by_ + bh && by_ < ay + ah {
                overlaps += 1;
            }
        }
    }
    let mut backward = 0;
    for n in &placed {
        let (nx, _, nw, _) = bx(n);
        for p in &n.pins {
            if p.cat == "exec" && p.dir == PinDir::Out {
                for l in &p.links {
                    if let Some(&(tx, _)) = r.pos.get(&l.node) {
                        if (tx as f64) < nx + nw {
                            backward += 1;
                        }
                    }
                }
            }
        }
    }
    (overlaps, backward)
}

/// `Math.round` for row bounds and other raw layout numbers.
pub fn js_round(x: f64) -> i64 {
    js::round_i(x)
}
