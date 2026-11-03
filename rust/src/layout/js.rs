//! The bits of JavaScript semantics the layout depends on, so the port gives the same numbers as the original.

use indexmap::IndexMap;

/// JS WhiteSpace + LineTerminator: what `String.prototype.trim` strips and what `\s` matches.
pub fn is_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0B}'
            | '\u{0C}'
            | '\r'
            | ' '
            | '\u{A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// The body of a regex character class equal to JS `\s` (the regex crate's `\s` differs on U+0085 and U+FEFF).
pub const WS_CLASS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// `String.prototype.trim`
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_ws)
}

/// `Math.round`: the nearest integer, halves toward +Infinity (not `f64::round`, which rounds halves away from 0).
pub fn round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// `Math.round(x)` as an integer coordinate (NaN becomes 0).
pub fn round_i(x: f64) -> i64 {
    round(x) as i64
}

/// `Math.max(a, b)`: NaN wins.
pub fn max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if b > a {
        b
    } else {
        a
    }
}

/// `Math.min(a, b)`: NaN wins.
pub fn min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if b < a {
        b
    } else {
        a
    }
}

/// `Math.max(init, ...xs)`; `Math.max()` of nothing is -Infinity, so pass `f64::NEG_INFINITY` for a bare spread.
pub fn max_of(init: f64, xs: impl IntoIterator<Item = f64>) -> f64 {
    xs.into_iter().fold(init, max)
}

/// `Math.min(init, ...xs)`; `Math.min()` of nothing is +Infinity.
pub fn min_of(init: f64, xs: impl IntoIterator<Item = f64>) -> f64 {
    xs.into_iter().fold(init, min)
}

/// Unary `+s` / `Number(s)` on a string: trimmed, "" is 0, decimal / hex / octal / binary / Infinity, else NaN.
pub fn number(s: &str) -> f64 {
    let t = trim(s);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = match t.get(..2) {
        Some("0x") | Some("0X") => 16,
        Some("0o") | Some("0O") => 8,
        Some("0b") | Some("0B") => 2,
        _ => 10,
    };
    if radix != 10 {
        let digits = &t[2..];
        if digits.is_empty() {
            return f64::NAN;
        }
        let mut v = 0f64;
        for c in digits.chars() {
            match c.to_digit(radix) {
                Some(d) => v = v * radix as f64 + d as f64,
                None => return f64::NAN,
            }
        }
        return v;
    }
    // StrDecimalLiteral: [+-] (digits [. digits?] | . digits) [(e|E) [+-] digits]
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut mantissa_digits = i - int_start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        mantissa_digits += i - frac_start;
    }
    if mantissa_digits == 0 {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return f64::NAN;
        }
    }
    if i != b.len() {
        return f64::NAN;
    }
    // Rust's parser wants a digit before an exponent and accepts "1." / ".5"; normalise the odd shapes first
    let mut norm = t.to_string();
    if let Some(p) = norm.find(['e', 'E']) {
        if norm[..p].ends_with('.') {
            norm.insert(p, '0');
        }
    }
    norm.parse::<f64>().unwrap_or(f64::NAN)
}

/// `String(x)` for a number, as a template literal prints it.
pub fn num_str(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x == 0.0 {
        return "0".into();
    }
    let a = x.abs();
    if (1e-6..1e21).contains(&a) {
        return format!("{x}");
    }
    let s = format!("{x:e}");
    match s.find('e') {
        Some(p) if !s[p + 1..].starts_with('-') => format!("{}e+{}", &s[..p], &s[p + 1..]),
        _ => s,
    }
}

/// `Array.prototype.sort` with a comparator: stable, and `a` goes after `b` only when `cmp(a, b) > 0`
/// (so a NaN result counts as equal, as in JS). Never panics on an inconsistent comparator.
pub fn sort_by<T: Clone>(v: &mut [T], cmp: &mut impl FnMut(&T, &T) -> f64) {
    let n = v.len();
    if n <= 1 {
        return;
    }
    let mid = n / 2;
    sort_by(&mut v[..mid], cmp);
    sort_by(&mut v[mid..], cmp);
    let left = v[..mid].to_vec();
    let right = v[mid..].to_vec();
    let (mut i, mut j, mut k) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        if cmp(&left[i], &right[j]) > 0.0 {
            v[k] = right[j].clone();
            j += 1;
        } else {
            v[k] = left[i].clone();
            i += 1;
        }
        k += 1;
    }
    for x in &left[i..] {
        v[k] = x.clone();
        k += 1;
    }
    for x in &right[j..] {
        v[k] = x.clone();
        k += 1;
    }
}

/// A canonical array index ("0", "17", never "017"): JS objects enumerate those keys first, ascending.
fn array_index(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    s.parse::<u64>().ok().filter(|v| *v < 4_294_967_295)
}

/// Keys in the order a JS object built by inserting them in this order would enumerate them.
pub fn object_key_order<'k>(keys: impl IntoIterator<Item = &'k str>) -> Vec<&'k str> {
    let keys: Vec<&str> = keys.into_iter().collect();
    if !keys.iter().any(|k| array_index(k).is_some()) {
        return keys;
    }
    let mut idx: Vec<(u64, &str)> = keys.iter().filter_map(|k| array_index(k).map(|i| (i, *k))).collect();
    idx.sort_by_key(|(i, _)| *i);
    idx.into_iter().map(|(_, k)| k).chain(keys.into_iter().filter(|k| array_index(k).is_none())).collect()
}

/// Reorder a map built by insertion into the order the equivalent JS object enumerates.
pub fn object_order<V>(mut m: IndexMap<String, V>) -> IndexMap<String, V> {
    if !m.keys().any(|k| array_index(k).is_some()) {
        return m;
    }
    let order: Vec<String> = object_key_order(m.keys().map(String::as_str)).into_iter().map(str::to_string).collect();
    order.into_iter().filter_map(|k| m.shift_remove(&k).map(|v| (k, v))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_parses_like_unary_plus() {
        assert_eq!(number(""), 0.0);
        assert_eq!(number("  -12 "), -12.0);
        assert_eq!(number("1."), 1.0);
        assert_eq!(number(".5"), 0.5);
        assert_eq!(number("1.e2"), 100.0);
        assert_eq!(number("0x1F"), 31.0);
        assert_eq!(number("1e3"), 1000.0);
        assert_eq!(number("-Infinity"), f64::NEG_INFINITY);
        assert!(number("inf").is_nan());
        assert!(number("12px").is_nan());
        assert!(number("-0x10").is_nan());
        assert!(number(".").is_nan());
    }

    #[test]
    fn js_round_and_strings() {
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(0.49999999999999994), 0.0);
        assert_eq!(num_str(430.5), "430.5");
        assert_eq!(num_str(-40.0), "-40");
        assert_eq!(num_str(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(num_str(1e21), "1e+21");
        assert_eq!(num_str(1.5e-7), "1.5e-7");
        assert_eq!(num_str(-0.0), "0");
    }

    #[test]
    fn js_sort_is_stable_and_object_order_puts_indices_first() {
        let mut v = vec![(1, 'a'), (0, 'b'), (1, 'c'), (0, 'd')];
        sort_by(&mut v, &mut |a, b| (a.0 - b.0) as f64);
        assert_eq!(v, vec![(0, 'b'), (0, 'd'), (1, 'a'), (1, 'c')]);
        assert_eq!(object_key_order(["b", "10", "a", "2", "02"]), vec!["2", "10", "b", "a", "02"]);
    }
}
