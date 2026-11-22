//! A tiny JSON reader (the core daemon only ever *writes* JSON, so the GUI
//! needs a reader on this side).  std only, no serde.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Arr(v) => Some(v),
            _ => None,
        }
    }
    pub fn strf(&self, key: &str) -> String {
        self.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
    pub fn f64f(&self, key: &str) -> f64 {
        self.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
    }
    pub fn boolf(&self, key: &str) -> bool {
        self.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
    }
    pub fn opt_f64(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|v| v.as_f64())
    }
    pub fn arrf(&self, key: &str) -> Vec<Value> {
        self.get(key)
            .and_then(|v| v.as_arr())
            .cloned()
            .unwrap_or_default()
    }
    /// convenience for diagnostics/tests
    pub fn as_map(&self) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        if let Value::Obj(pairs) = self {
            for (k, v) in pairs.iter() {
                m.insert(k.clone(), v.clone());
            }
        }
        m
    }
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!("json at byte {}: {}", self.i, msg))
    }

    fn ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.err(&format!("expected '{}'", c as char))
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        if self.b.len() >= self.i + word.len() && &self.b[self.i..self.i + word.len()] == word.as_bytes()
        {
            self.i += word.len();
            Ok(())
        } else {
            self.err(&format!("expected {}", word))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => {
                self.literal("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Value::Null)
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => self.err("unexpected character"),
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut pairs: Vec<(String, Value)> = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Obj(pairs));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            self.expect(b':')?;
            let v = self.value()?;
            pairs.push((key, v));
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return self.err("expected , or }"),
            }
        }
        Ok(Value::Obj(pairs))
    }

    fn array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items: Vec<Value> = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            let v = self.value()?;
            items.push(v);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return self.err("expected , or ]"),
            }
        }
        Ok(Value::Arr(items))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    if self.i >= self.b.len() {
                        return self.err("bad escape");
                    }
                    let e = self.b[self.i];
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                // surrogate pair
                                if self.i + 1 < self.b.len()
                                    && self.b[self.i] == b'\\'
                                    && self.b[self.i + 1] == b'u'
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    let combined =
                                        0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    if let Some(ch) = char::from_u32(combined) {
                                        out.push(ch);
                                    }
                                } else {
                                    out.push('\u{fffd}');
                                }
                            } else if let Some(ch) = char::from_u32(cp) {
                                out.push(ch);
                            } else {
                                out.push('\u{fffd}');
                            }
                        }
                        _ => return self.err("unknown escape"),
                    }
                }
                _ => {
                    // collect the whole utf-8 sequence for this byte
                    let start = self.i - 1;
                    let extra = match self.b[start] {
                        b if b < 0x80 => 0,
                        b if b >= 0xC0 && b < 0xE0 => 1,
                        b if b >= 0xE0 && b < 0xF0 => 2,
                        b if b >= 0xF0 => 3,
                        _ => 0,
                    };
                    let end = (start + 1 + extra).min(self.b.len());
                    out.push_str(String::from_utf8_lossy(&self.b[start..end]).as_ref());
                    self.i = end;
                }
            }
        }
        self.err("unterminated string")
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.i + 4 > self.b.len() {
            return self.err("bad \\u escape");
        }
        let text = std::str::from_utf8(&self.b[self.i..self.i + 4]).map_err(|_| "bad \\u".to_string())?;
        let v = u32::from_str_radix(text, 16).map_err(|_| "bad \\u digits")?;
        self.i += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        if self.peek() == Some(b'-') || self.peek() == Some(b'+') {
            self.i += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'-' || c == b'+' {
                self.i += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "bad number")?;
        text.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| format!("bad number {:?}", text))
    }
}

pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = P {
        b: text.as_bytes(),
        i: 0,
    };
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return p.err("trailing characters");
    }
    Ok(v)
}

pub fn selftest() -> i32 {
    let mut fails = 0usize;
    let mut total = 0usize;
    let mut t = |name: &str, ok: bool| {
        total += 1;
        if !ok {
            fails += 1;
            println!("  FAIL  {}", name);
        } else {
            println!("  ok    {}", name);
        }
    };

    let v = parse(r#"{"a":"b","n":1.5,"t":true,"z":null,"arr":[1,"two",[3]],"o":{"k":"v"}}"#);
    match v {
        Ok(v) => {
            t("object/keys", v.strf("a") == "b" && (v.f64f("n") - 1.5).abs() < 1e-9 && v.boolf("t"));
            t("null is not a bool", !v.boolf("z"));
            t("nested array", v.arrf("arr").len() == 3);
            t(
                "nested object",
                v.get("o").and_then(|o| o.get("k")).and_then(|k| k.as_str()) == Some("v"),
            );
        }
        Err(e) => {
            println!("  FAIL  parse basic: {}", e);
            t("parse basic", false);
        }
    }
    t(
        "escapes",
        parse(r#""a\"b\nc\u00e9""#).map(|v| v.as_str() == Some("a\"b\nc\u{e9}")).unwrap_or(false),
    );
    t(
        "surrogate pair",
        parse(r#""\ud83d\ude00""#)
            .map(|v| v.as_str() == Some("\u{1f600}"))
            .unwrap_or(false),
    );
    t("array at top level", parse("[1,2,3]").map(|v| v.as_arr().map(|a| a.len())) == Ok(Some(3)));
    t("rejects trailing junk", parse("{} }").is_err());
    t("rejects bare word", parse("nul").is_err());
    t("empty object", parse("{}").map(|v| matches!(v, Value::Obj(_))) == Ok(true));

    // the real payload shape the daemon emits
    let sample = r#"{"app":"sonos-bridge","version":"0.1.0","mode":"sonos","players":[{"name":"Living Room","udn":"uuid:A","ip":"192.168.1.21","volume":18},{"name":"Kitchen","udn":"uuid:B","volume":null}],"peak":0.3,"audio_active":true}"#;
    match parse(sample) {
        Ok(v) => {
            let players = v.arrf("players");
            t("state: players array", players.len() == 2);
            t(
                "state: optional volume",
                players[0].opt_f64("volume") == Some(18.0) && players[1].opt_f64("volume").is_none(),
            );
            t("state: mode", v.strf("mode") == "sonos");
        }
        Err(e) => {
            println!("  FAIL  state sample: {}", e);
            t("state sample", false);
        }
    }

    println!();
    if fails == 0 {
        println!("json: all {} checks passed", total);
        0
    } else {
        println!("json: {}/{} checks FAILED", fails, total);
        1
    }
}
