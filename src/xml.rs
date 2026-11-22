//! Minimal XML reading - just enough for Sonos device descriptions and
//! ZoneGroupState.  No namespaces, no validation, tolerant of junk.

use crate::util::xml_unescape;

/// True if `xml[start..]` begins with `<name` and the name really ends there
/// (i.e. `<name>` or `<name attr=..>` - not `<nameSomething>`).
fn name_boundary(xml: &str, start: usize, name: &str) -> Option<usize> {
    let rest = &xml[start..];
    let open = format!("<{}", name);
    if !rest.starts_with(&open) {
        return None;
    }
    let after = &rest[open.len()..];
    if after.is_empty() {
        return None;
    }
    let c = after.as_bytes()[0];
    if c == b'>' || c == b' ' || c == b'/' {
        Some(start + open.len())
    } else {
        None
    }
}

/// First text content of `<name>...</name>` (unescaped).
pub fn tag_value(xml: &str, name: &str) -> Option<String> {
    let bytes = xml.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if let Some(at) = name_boundary(xml, i, name) {
            let after_gt = match xml[at..].find('>') {
                Some(g) => at + g + 1,
                None => return None,
            };
            let close = format!("</{}", name);
            let end = xml[after_gt..].find(&close);
            let inner = match end {
                Some(e) => &xml[after_gt..after_gt + e],
                None => "",
            };
            return Some(xml_unescape(inner.trim()));
        }
        i += 1;
    }
    None
}

/// Raw (still escaped) inner XML of the first `<name>...</name>`.
pub fn tag_raw(xml: &str, name: &str) -> Option<String> {
    let bytes = xml.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(at) = name_boundary(xml, i, name) {
                let after_gt = match xml[at..].find('>') {
                    Some(g) => at + g + 1,
                    None => return None,
                };
                let close = format!("</{}", name);
                if let Some(e) = xml[after_gt..].find(&close) {
                    return Some(xml[after_gt..after_gt + e].to_string());
                }
                return None;
            }
        }
        i += 1;
    }
    None
}

/// Every `<name>...</name>` in document order.
pub fn all_tag_values(xml: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let open = format!("<{}", name);
    let mut from = 0usize;
    while let Some(rel) = xml[from..].find(&open) {
        let at = from + rel;
        if let Some(at) = name_boundary(xml, at, name) {
            let after_gt = match xml[at..].find('>') {
                Some(g) => at + g + 1,
                None => break,
            };
            let close = format!("</{}", name);
            match xml[after_gt..].find(&close) {
                Some(e) => {
                    out.push(xml_unescape(xml[after_gt..after_gt + e].trim()));
                    from = after_gt + e + close.len();
                }
                None => break,
            }
        } else {
            from = at + open.len();
        }
    }
    out
}

/// Byte offsets where a complete `<tag` starts (name ends there, so `<ZoneGroup`
/// does not match `<ZoneGroups>` or `<ZoneGroupState>`).
pub fn tag_positions(xml: &str, tag: &str) -> Vec<usize> {
    let bytes = xml.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if name_boundary(xml, i, tag).is_some() {
                out.push(i);
            }
        }
        i += 1;
    }
    out
}

/// Every `attr="value"` of every `<tag ...>` in document order.
pub fn all_attr_values(xml: &str, tag: &str, attr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = format!("{}=", attr);
    for at in tag_positions(xml, tag) {
        let gt = match xml[at..].find('>') {
            Some(g) => at + g,
            None => continue,
        };
        let attrs = &xml[at..gt];
        let mut from = 0usize;
        while let Some(rel) = attrs[from..].find(&needle) {
            let p = from + rel;
            // the attribute name must start at a whitespace boundary
            let boundary = p == 0 || matches!(attrs.as_bytes()[p - 1], b' ' | b'\t' | b'\n');
            let rest = &attrs[p + needle.len()..];
            let quote = rest.as_bytes().first().copied();
            if boundary && matches!(quote, Some(b'"') | Some(b'\'')) {
                if let Some(end) = rest[1..].find(quote.unwrap() as char) {
                    out.push(xml_unescape(&rest[1..1 + end]));
                    break;
                }
            }
            from = p + needle.len();
        }
    }
    out
}

/// `<tag ... attr="value" ...>` - first match.
pub fn attr_value(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let bytes = xml.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(at) = name_boundary(xml, i, tag) {
                let gt = match xml[at..].find('>') {
                    Some(g) => at + g,
                    None => return None,
                };
                let attrs = &xml[at..gt];
                let needle = format!("{}=", attr);
                if let Some(p) = attrs.find(&needle) {
                    let rest = &attrs[p + needle.len()..];
                    if rest.is_empty() {
                        return None;
                    }
                    let quote = rest.as_bytes()[0];
                    if quote == b'"' || quote == b'\'' {
                        if let Some(end) = rest[1..].find(quote as char) {
                            return Some(xml_unescape(&rest[1..1 + end]));
                        }
                    }
                }
                return None;
            }
        }
        i += 1;
    }
    None
}
