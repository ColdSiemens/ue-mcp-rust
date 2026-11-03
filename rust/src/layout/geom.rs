//! Estimated on-screen size of K2 graph nodes at 1:1 zoom, and where their pins sit. Port of `scripts/lib/geom.mjs`.
//! The editor never stores node sizes, so this models the default UE 5.5 node style (Roboto, 96 dpi). The constants
//! were measured from editor screenshots of real nodes; re-measure them if the editor style changes.
//! `node.title` and `pin.text` come from read_blueprint_graph (see `apply_graph_info`).

use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

use super::js;
use super::t3d::{Node, Pin, PinDir};

#[derive(Debug, Clone, PartialEq)]
pub struct Geom {
    pub w: f64,
    pub h: f64,
    pub exec_y: f64,
    /// Pin id -> y of the pin's centre, relative to the node's top.
    pub pin_y: IndexMap<String, f64>,
}

#[allow(dead_code)]
mod m {
    pub const TITLE: f64 = 26.0; // header height
    pub const TITLE_LINE: f64 = 14.5; // plus each extra line ("Target is Text", "Custom Event")
    pub const TOP: f64 = 2.0; // body padding above the first pin row
    pub const BOTTOM: f64 = 3.0; // and below the last
    pub const ROW_EXEC: f64 = 33.0;
    pub const ROW_DATA: f64 = 30.5;
    pub const ROW_EDIT: f64 = 34.0;
    pub const ADV_ROW: f64 = 21.0;
    pub const IN_SIDE: f64 = 34.0; // node edge + pin icon + gap, before an input label
    pub const OUT_SIDE: f64 = 33.0; // after an output label: gap + pin icon + node edge
    pub const BARE: f64 = 30.0; // a pin with no label (exec in/out)
    pub const COL_GAP: f64 = 34.0; // between the input and the output column
    pub const EDIT_GAP: f64 = 8.0; // input label -> its inline default-value editor
    pub const TITLE_CHROME: f64 = 54.0; // header padding + icon
    pub const EVENT_PIN: f64 = 26.0; // the delegate pin events show in the header
    pub const GETTER_CHROME: f64 = 52.0;
    pub const GETTER_TARGET: f64 = 51.0;
    pub const GETTER_H: f64 = 32.0;
    pub const GETTER_TARGET_H: f64 = 36.0;
    pub const COMPACT_BASE: f64 = 118.0;
    pub const COMPACT_MIN: f64 = 123.0;
    pub const COMPACT_ROW: f64 = 24.0;
    pub const COMPACT_PAD: f64 = 9.0;
    pub const KNOT_W: f64 = 42.0;
    pub const KNOT_H: f64 = 24.0;
}

/// Roboto advance widths in em.
fn adv(c: char) -> Option<f64> {
    Some(match c {
        'a' => 0.544, 'b' => 0.561, 'c' => 0.523, 'd' => 0.564, 'e' => 0.530, 'f' => 0.347, 'g' => 0.561,
        'h' => 0.551, 'i' => 0.243, 'j' => 0.239, 'k' => 0.496, 'l' => 0.243, 'm' => 0.876,
        'n' => 0.552, 'o' => 0.570, 'p' => 0.561, 'q' => 0.568, 'r' => 0.338, 's' => 0.516, 't' => 0.327,
        'u' => 0.551, 'v' => 0.484, 'w' => 0.751, 'x' => 0.496, 'y' => 0.473, 'z' => 0.496,
        'A' => 0.652, 'B' => 0.623, 'C' => 0.651, 'D' => 0.656, 'E' => 0.568, 'F' => 0.553, 'G' => 0.681,
        'H' => 0.713, 'I' => 0.272, 'J' => 0.552, 'K' => 0.627, 'L' => 0.538, 'M' => 0.873,
        'N' => 0.713, 'O' => 0.688, 'P' => 0.631, 'Q' => 0.688, 'R' => 0.616, 'S' => 0.595, 'T' => 0.597,
        'U' => 0.649, 'V' => 0.636, 'W' => 0.887, 'X' => 0.627, 'Y' => 0.601, 'Z' => 0.599,
        ' ' => 0.248, '(' => 0.342, ')' => 0.348, '_' => 0.452, '-' => 0.276, '.' => 0.264, ',' => 0.196,
        ':' => 0.242, '/' => 0.412, '{' => 0.338, '}' => 0.338,
        '%' => 0.733, '$' => 0.562, '?' => 0.473, '!' => 0.258, '=' => 0.549, '+' => 0.567, '<' => 0.508,
        '>' => 0.522, '&' => 0.622, '*' => 0.431, '#' => 0.616,
        '\'' => 0.175, '"' => 0.320, '|' => 0.244, '×' => 0.53, '÷' => 0.57, '•' => 0.3,
        _ => return None,
    })
}

/// Width in px of `s` in Roboto at `size` pt.
pub fn text_w(s: &str, size: f64, bold: bool) -> f64 {
    let mut w = 0.0;
    for ch in s.chars() {
        w += adv(ch).unwrap_or(if ch.is_ascii_digit() { 0.562 } else { 0.6 });
    }
    w * size * 96.0 / 72.0 * (if bold { 1.06 } else { 1.0 })
}

static RE_GUID_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_[0-9]+_[0-9A-F]{32}$").unwrap());
static RE_B_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^b[A-Z]").unwrap());
static RE_LOWER_UPPER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z])([A-Z])").unwrap());
static RE_ACRONYM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([A-Z]+)([A-Z][a-z])").unwrap());
static RE_LETTER_DIGIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-zA-Z])([0-9])").unwrap());
static RE_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#""((?:[^"\\]|\\[^\n\r\x{{2028}}\x{{2029}}])*)"[{ws}]*\)?[{ws}]*$"#, ws = js::WS_CLASS)).unwrap()
});
static RE_MEMBER_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"MemberName="([^"]+)""#).unwrap());
static RE_QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).unwrap());

/// "bIsLocked" -> "Is Locked", "ItemName_5_<guid>" -> "Item Name"
pub fn spaced(name: &str) -> String {
    let mut s = RE_GUID_SUFFIX.replace(name, "").into_owned();
    if RE_B_PREFIX.is_match(&s) {
        s = s[1..].to_string();
    }
    let s = s.replace('_', " ");
    let s = RE_LOWER_UPPER.replace_all(&s, "${1} ${2}");
    let s = RE_ACRONYM.replace_all(&s, "${1} ${2}");
    let s = RE_LETTER_DIGIT.replace_all(&s, "${1} ${2}");
    js::trim(&s).to_string()
}

/// The visible string of NSLOCTEXT(...)/INVTEXT(...); LOCTABLE needs the string table, so None.
fn literal(v: Option<&str>) -> Option<String> {
    let v = v?;
    if v.is_empty() || v.starts_with("LOCTABLE(") {
        return None;
    }
    Some(match RE_LITERAL.captures(v) {
        Some(c) => c.get(1).map_or("", |g| g.as_str()).to_string(),
        None => v.to_string(),
    })
}

/// `literal(v) || fallback`
fn literal_or(v: Option<&str>, fallback: impl FnOnce() -> String) -> String {
    literal(v).filter(|s| !s.is_empty()).unwrap_or_else(fallback)
}

pub(super) fn member_name(r: &str) -> Option<&str> {
    RE_MEMBER_NAME.captures(r).and_then(|c| c.get(1)).map(|g| g.as_str())
}

const COMPACT_PREFIX: [(&str, &str); 11] = [
    ("EqualEqual_", "=="),
    ("NotEqual_", "!="),
    ("GreaterEqual_", ">="),
    ("LessEqual_", "<="),
    ("Greater_", ">"),
    ("Less_", "<"),
    ("Add_", "+"),
    ("Subtract_", "-"),
    ("Multiply_", "×"),
    ("Divide_", "÷"),
    ("Percent_", "%"),
];
const COMPACT_EXACT: [(&str, &str); 9] = [
    ("BooleanAND", "AND"),
    ("BooleanOR", "OR"),
    ("BooleanXOR", "XOR"),
    ("BooleanNAND", "NAND"),
    ("BooleanNOR", "NOR"),
    ("Not_PreBool", "NOT"),
    ("Array_Length", "LENGTH"),
    ("Array_Contains", "CONTAINS"),
    ("Array_Get", "GET"),
];

fn compact_title(n: &Node) -> Option<&'static str> {
    if n.cls == "K2Node_PromotableOperator" {
        return Some("+");
    }
    if !matches!(
        n.cls.as_str(),
        "K2Node_CallFunction" | "K2Node_CommutativeAssociativeBinaryOperator" | "K2Node_CallArrayFunction"
    ) {
        return None;
    }
    let r = n.props.get("FunctionReference").map_or("", String::as_str);
    let f = member_name(r)?;
    // autocasts with CompactNodeTitle "->" draw as a small dot node
    if f.starts_with("Conv_")
        && (r.contains("KismetStringLibrary") || r.contains("KismetMathLibrary") || f == "Conv_TextToString")
    {
        return Some("•");
    }
    // the JS table is checked in order; prefix and exact entries never match the same name
    for (p, t) in COMPACT_PREFIX {
        if f.starts_with(p) {
            return Some(t);
        }
    }
    COMPACT_EXACT.iter().find(|(e, _)| f == *e).map(|(_, t)| *t)
}

fn vis(def: &str) -> Option<&'static str> {
    match def {
        "HitTestInvisible" => Some("Not Hit-Testable (Self & All Children)"),
        "SelfHitTestInvisible" => Some("Not Hit-Testable (Self Only)"),
        _ => None,
    }
}

fn obj_path(s: &str) -> &str {
    RE_QUOTED.captures(s).and_then(|c| c.get(1)).map_or(s, |g| g.as_str())
}

fn editor_w(p: &Pin) -> f64 {
    if p.dir != PinDir::In || !p.links.is_empty() || p.cat == "exec" || p.hidden {
        return 0.0;
    }
    if p.name == "self" {
        return text_w("self", 9.0, false) + 10.0;
    }
    if !p.container.is_empty() && p.container != "None" {
        return 0.0;
    }
    let def = p.def.as_deref().unwrap_or("");
    match p.cat.as_str() {
        "bool" => 22.0,
        "int" | "int64" | "real" | "float" | "double" => {
            js::max(40.0, text_w(if def.is_empty() { "0" } else { def }, 9.0, false) + 30.0)
        }
        "byte" | "enum" => {
            if !p.sub_obj.is_empty() && p.sub_obj != "None" {
                let label = match vis(def) {
                    Some(v) => v.to_string(),
                    None => Some(spaced(def)).filter(|s| !s.is_empty()).unwrap_or_else(|| "None".to_string()),
                };
                js::max(64.0, text_w(&label, 9.0, false) + 40.0)
            } else {
                40.0
            }
        }
        "name" | "string" => js::min(320.0, js::max(40.0, text_w(def, 9.0, false) + 30.0)),
        "text" => {
            let s = match &p.text {
                Some(t) => t.clone(),
                None => literal(p.def_text.as_deref()).unwrap_or_else(|| def.to_string()),
            };
            js::min(400.0, js::max(52.0, text_w(&s, 9.0, false) + 34.0))
        }
        "object" | "softobject" => {
            let path = obj_path(&p.sub_obj);
            let actorish = ["Actor", "Pawn", "Character", "Component", "Controller"].iter().any(|e| path.ends_with(e))
                || path.contains("/BP_");
            if actorish {
                0.0
            } else {
                200.0
            }
        }
        "class" | "softclass" => 190.0,
        "struct" => {
            let path = obj_path(&p.sub_obj);
            if path.ends_with("LinearColor") || path.ends_with(".Color") || path.ends_with("SlateColor") {
                56.0
            } else if path.ends_with(".Vector") || path.ends_with(".Rotator") {
                230.0
            } else if path.ends_with(".Vector2D") {
                150.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

fn label_of(n: &Node, p: &Pin) -> String {
    if p.cat == "exec" {
        if n.cls == "K2Node_IfThenElse" {
            return match p.name.as_str() {
                "then" => "True",
                "else" => "False",
                _ => "",
            }
            .to_string();
        }
        if p.name == "execute" || p.name == "then" {
            return String::new();
        }
        return literal_or(p.friendly.as_deref(), || spaced(&p.name));
    }
    if p.name == "self" {
        return "Target".to_string();
    }
    if n.cls == "K2Node_VariableSet" && p.dir == PinDir::Out {
        return String::new();
    }
    if p.name == "ReturnValue" {
        return "Return Value".to_string();
    }
    literal_or(p.friendly.as_deref(), || spaced(&p.name))
}

fn is_event_class(cls: &str) -> bool {
    matches!(
        cls,
        "K2Node_Event"
            | "K2Node_CustomEvent"
            | "K2Node_ComponentBoundEvent"
            | "K2Node_ActorBoundEvent"
            | "K2Node_InputAction"
            | "K2Node_InputKey"
            | "K2Node_EnhancedInputAction"
    )
}

pub fn visible_pins(n: &Node) -> Vec<&Pin> {
    let adv_hidden = n.props.get("AdvancedPinDisplay").is_some_and(|v| v == "Hidden");
    n.pins.iter().filter(|p| !p.hidden && !(adv_hidden && p.adv)).collect()
}

/// `n.title || n.cls`
pub fn title_or_cls(n: &Node) -> &str {
    n.title.as_deref().filter(|t| !t.is_empty()).unwrap_or(&n.cls)
}

pub fn geom(n: &Node) -> Geom {
    let lines: Vec<&str> = title_or_cls(n).split('\n').collect();
    let pins = visible_pins(n);
    let mut pin_y: IndexMap<String, f64> = IndexMap::new();
    if n.cls == "K2Node_Knot" {
        for p in &n.pins {
            pin_y.insert(p.id.clone(), m::KNOT_H / 2.0);
        }
        return Geom { w: m::KNOT_W, h: m::KNOT_H, exec_y: m::KNOT_H / 2.0, pin_y };
    }
    let has_exec = pins.iter().any(|p| p.cat == "exec");
    if n.cls == "K2Node_VariableGet" && !has_exec {
        let name = spaced(lines[0].strip_prefix("Get ").unwrap_or(lines[0]));
        let target = pins.iter().find(|p| p.name == "self");
        let w = text_w(&name, 9.0, false)
            + m::GETTER_CHROME
            + match target {
                Some(s) => text_w("Target", 9.0, false) + m::GETTER_TARGET + editor_w(s),
                None => 0.0,
            };
        let h = if target.is_some() { m::GETTER_TARGET_H } else { m::GETTER_H };
        for p in &n.pins {
            pin_y.insert(p.id.clone(), h / 2.0);
        }
        return Geom { w: w.ceil(), h, exec_y: h / 2.0, pin_y };
    }
    let left: Vec<&Pin> = pins.iter().copied().filter(|p| p.dir == PinDir::In).collect();
    let right: Vec<&Pin> = pins.iter().copied().filter(|p| p.dir == PinDir::Out).collect();
    let has_adv = matches!(n.props.get("AdvancedPinDisplay").map(String::as_str), Some("Hidden") | Some("Shown"));
    if let Some(ct) = compact_title(n).filter(|_| !has_exec && !has_adv) {
        let h = (left.len().max(right.len()).max(1)) as f64 * m::COMPACT_ROW + m::COMPACT_PAD;
        for side in [&left, &right] {
            for (i, p) in side.iter().enumerate() {
                let y = (h - side.len() as f64 * m::COMPACT_ROW) / 2.0 + (i as f64 + 0.5) * m::COMPACT_ROW;
                pin_y.insert(p.id.clone(), y);
            }
        }
        let eds = js::max_of(0.0, left.iter().map(|p| editor_w(p)));
        let w = js::max(m::COMPACT_MIN, m::COMPACT_BASE + text_w(ct, 16.0, true))
            + if eds != 0.0 { eds + m::EDIT_GAP } else { 0.0 };
        return Geom { w: w.ceil(), h, exec_y: h / 2.0, pin_y };
    }
    let row_h = |p: &Pin| {
        if p.cat == "exec" {
            m::ROW_EXEC
        } else if editor_w(p) > 0.0 && p.cat != "bool" {
            m::ROW_EDIT
        } else {
            m::ROW_DATA
        }
    };
    let title_h = m::TITLE + m::TITLE_LINE * (lines.len() as f64 - 1.0);
    let mut place = |side: &[&Pin]| {
        let mut y = title_h + m::TOP;
        for p in side {
            pin_y.insert(p.id.clone(), y + row_h(p) / 2.0);
            y += row_h(p);
        }
        y
    };
    let end_left = place(&left);
    let end_right = place(&right);
    let end = js::max(js::max(end_left, end_right), title_h + m::TOP + m::ROW_DATA);
    let (extra_h, min_w) = if n.cls == "K2Node_CreateDelegate" { (m::ROW_EDIT, 230.0) } else { (0.0, 0.0) };
    let h = end + (if has_adv { m::ADV_ROW } else { 0.0 }) + extra_h + m::BOTTOM;
    let in_w = |p: &Pin| {
        let lab = label_of(n, p);
        let ed = editor_w(p);
        (if !lab.is_empty() { m::IN_SIDE + text_w(&lab, 9.0, false) } else { m::BARE })
            + if ed != 0.0 { m::EDIT_GAP + ed } else { 0.0 }
    };
    let out_w = |p: &Pin| {
        let lab = label_of(n, p);
        if !lab.is_empty() {
            text_w(&lab, 9.0, false) + m::OUT_SIDE
        } else {
            m::BARE
        }
    };
    let title = if n.cls == "K2Node_VariableSet" { "SET" } else { lines[0] };
    let title_w = js::max_of(text_w(title, 10.0, true), lines[1..].iter().map(|l| text_w(l, 9.0, false)))
        + m::TITLE_CHROME
        + if is_event_class(&n.cls) { m::EVENT_PIN } else { 0.0 };
    let w = js::max(
        js::max(
            title_w,
            js::max_of(0.0, left.iter().map(|p| in_w(p))) + m::COL_GAP + js::max_of(0.0, right.iter().map(|p| out_w(p))),
        ),
        min_w,
    );
    let first_exec = pins.iter().find(|p| p.cat == "exec");
    let exec_y = match first_exec {
        Some(p) => pin_y.get(&p.id).copied().unwrap_or(f64::NAN),
        None => title_h + m::TOP + m::ROW_EXEC / 2.0,
    };
    Geom { w: w.ceil(), h: h.ceil(), exec_y, pin_y }
}
