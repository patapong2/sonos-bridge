//! Tiny JSON *writer* (we only ever emit JSON; nothing is parsed).

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push(c),
        }
    }
    out
}

pub fn s(v: &str) -> String {
    format!("\"{}\"", esc(v))
}

pub fn f(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    format!("{:.4}", v)
}

pub fn arr(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

/// Simple insertion-ordered object builder.
pub struct Obj {
    parts: Vec<String>,
}

impl Obj {
    pub fn new() -> Obj {
        Obj { parts: Vec::new() }
    }
    pub fn str(&mut self, key: &str, value: &str) -> &mut Obj {
        self.parts.push(format!("{}:{}", s(key), s(value)));
        self
    }
    #[allow(clippy::should_implement_trait)]
    pub fn bool(&mut self, key: &str, value: bool) -> &mut Obj {
        self.parts.push(format!("{}:{}", s(key), if value { "true" } else { "false" }));
        self
    }
    pub fn num(&mut self, key: &str, value: i64) -> &mut Obj {
        self.parts.push(format!("{}:{}", s(key), value));
        self
    }
    pub fn float(&mut self, key: &str, value: f64) -> &mut Obj {
        self.parts.push(format!("{}:{}", s(key), f(value)));
        self
    }
    /// raw already-encoded JSON value
    pub fn raw(&mut self, key: &str, value: &str) -> &mut Obj {
        self.parts.push(format!("{}:{}", s(key), value));
        self
    }
    pub fn done(&self) -> String {
        format!("{{{}}}", self.parts.join(","))
    }
}

impl Default for Obj {
    fn default() -> Obj {
        Obj::new()
    }
}
