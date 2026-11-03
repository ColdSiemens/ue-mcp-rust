//! Blueprint graph layout, ported from the bp-graph-layout skill (`scripts/lib/*.mjs` and the pure parts of
//! `scripts/bp_layout.mjs`). The port reproduces the JS output exactly; the parity tests run the original's
//! answers on captured fixtures (`tests/fixtures/bp_layout`).
//!
//! parse_t3d (export_nodes_t3d text) -> apply_graph_info (read_blueprint_graph titles / text values)
//! -> layout_graph -> pos per node name; check() and preview_svg() report on the result.


mod engine;
mod geom;
mod js;
mod preview;
mod t3d;

// The full layout API stays public (tuning via Opt, geometry for previews); the server uses part of it.
#[allow(unused_imports)]
pub use engine::{check, js_round, layout_graph, layout_graph_with, LayoutResult, Opt, Row, OPT};
#[allow(unused_imports)]
pub use geom::{geom, text_w, Geom};
pub use preview::preview_svg;
#[allow(unused_imports)]
pub use t3d::{apply_graph_info, foreign_classes, parse_t3d, Link, Node, Pin, PinDir};

#[cfg(test)]
mod tests;
