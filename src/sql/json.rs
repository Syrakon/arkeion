//! Mini-JSON en Rust puro (sin dependencias, supply-chain mínima D8): parsea,
//! serializa de forma compacta y evalúa rutas tipo SQLite (`$.a.b[0]`). Solo lo
//! que necesitan las funciones `json_*` del dialecto; no es un JSON de propósito
//! general. No conoce `record::Value`: las conversiones SQL↔JSON viven en `exec`.

/// Un valor JSON. Los objetos conservan el orden de inserción (como SQLite).
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Nombre de tipo estilo `json_type` de SQLite.
pub fn type_name(j: &Json) -> &'static str {
    match j {
        Json::Null => "null",
        Json::Bool(true) => "true",
        Json::Bool(false) => "false",
        Json::Int(_) => "integer",
        Json::Float(_) => "real",
        Json::Str(_) => "text",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

/// Parsea un documento JSON completo. `None` si es inválido o sobran caracteres.
pub fn parse(s: &str) -> Option<Json> {
    let chars: Vec<char> = s.chars().collect();
    let mut p = Parser { c: &chars, i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    (p.i == p.c.len()).then_some(v)
}

/// Serialización compacta (sin espacios), reusable y estable.
pub fn to_string(j: &Json) -> String {
    let mut s = String::new();
    write(j, &mut s);
    s
}

/// Navega una ruta `$.a.b[0]` desde `root`. `None` si la ruta no existe o es
/// inválida (debe empezar por `$`).
pub fn extract<'a>(root: &'a Json, path: &str) -> Option<&'a Json> {
    let c: Vec<char> = path.chars().collect();
    if c.first() != Some(&'$') {
        return None;
    }
    let mut i = 1;
    let mut cur = root;
    while i < c.len() {
        match c[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < c.len() && c[i] != '.' && c[i] != '[' {
                    i += 1;
                }
                let key: String = c[start..i].iter().collect();
                let Json::Object(o) = cur else { return None };
                cur = &o.iter().find(|(k, _)| *k == key)?.1;
            }
            '[' => {
                i += 1;
                let start = i;
                while i < c.len() && c[i] != ']' {
                    i += 1;
                }
                let idx: usize = c[start..i].iter().collect::<String>().parse().ok()?;
                i += 1; // salta ']'
                let Json::Array(a) = cur else { return None };
                cur = a.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

fn write(j: &Json, out: &mut String) {
    match j {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(n) => out.push_str(&n.to_string()),
        Json::Float(f) => out.push_str(&format!("{f:?}")),
        Json::Str(s) => write_str(s, out),
        Json::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(v, out);
            }
            out.push(']');
        }
        Json::Object(o) => {
            out.push('{');
            for (i, (k, v)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(k, out);
                out.push(':');
                write(v, out);
            }
            out.push('}');
        }
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    c: &'a [char],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }
    fn value(&mut self) -> Option<Json> {
        self.ws();
        match self.peek()? {
            '{' => self.object(),
            '[' => self.array(),
            '"' => self.string().map(Json::Str),
            't' => self.lit("true", Json::Bool(true)),
            'f' => self.lit("false", Json::Bool(false)),
            'n' => self.lit("null", Json::Null),
            c if c == '-' || c.is_ascii_digit() => self.number(),
            _ => None,
        }
    }
    fn lit(&mut self, word: &str, val: Json) -> Option<Json> {
        for ch in word.chars() {
            if self.bump()? != ch {
                return None;
            }
        }
        Some(val)
    }
    fn string(&mut self) -> Option<String> {
        if self.bump()? != '"' {
            return None;
        }
        let mut s = String::new();
        loop {
            match self.bump()? {
                '"' => return Some(s),
                '\\' => match self.bump()? {
                    '"' => s.push('"'),
                    '\\' => s.push('\\'),
                    '/' => s.push('/'),
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    'b' => s.push('\u{8}'),
                    'f' => s.push('\u{c}'),
                    'u' => {
                        let mut code = 0u32;
                        for _ in 0..4 {
                            code = code * 16 + self.bump()?.to_digit(16)?;
                        }
                        s.push(char::from_u32(code)?);
                    }
                    _ => return None,
                },
                c => s.push(c),
            }
        }
    }
    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        if self.peek() == Some('-') {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            while matches!(p.peek(), Some(c) if c.is_ascii_digit()) {
                p.i += 1;
            }
        };
        digits(self);
        let mut is_float = false;
        if self.peek() == Some('.') {
            is_float = true;
            self.i += 1;
            digits(self);
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.i += 1;
            }
            digits(self);
        }
        let text: String = self.c[start..self.i].iter().collect();
        if is_float {
            text.parse::<f64>().ok().map(Json::Float)
        } else {
            text.parse::<i64>()
                .ok()
                .map(Json::Int)
                .or_else(|| text.parse::<f64>().ok().map(Json::Float))
        }
    }
    fn array(&mut self) -> Option<Json> {
        self.bump(); // '['
        let mut v = Vec::new();
        self.ws();
        if self.peek() == Some(']') {
            self.bump();
            return Some(Json::Array(v));
        }
        loop {
            v.push(self.value()?);
            self.ws();
            match self.bump()? {
                ',' => {}
                ']' => return Some(Json::Array(v)),
                _ => return None,
            }
        }
    }
    fn object(&mut self) -> Option<Json> {
        self.bump(); // '{'
        let mut o = Vec::new();
        self.ws();
        if self.peek() == Some('}') {
            self.bump();
            return Some(Json::Object(o));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            if self.bump()? != ':' {
                return None;
            }
            o.push((k, self.value()?));
            self.ws();
            match self.bump()? {
                ',' => {}
                '}' => return Some(Json::Object(o)),
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip_and_extract() {
        let j = parse(r#"{"a": 1, "b": [10, 20, {"c": "hi"}], "d": null, "e": true}"#).unwrap();
        assert_eq!(extract(&j, "$.a"), Some(&Json::Int(1)));
        assert_eq!(extract(&j, "$.b[1]"), Some(&Json::Int(20)));
        assert_eq!(extract(&j, "$.b[2].c"), Some(&Json::Str("hi".into())));
        assert_eq!(extract(&j, "$.d"), Some(&Json::Null));
        assert_eq!(extract(&j, "$.e"), Some(&Json::Bool(true)));
        assert_eq!(extract(&j, "$.missing"), None);
        assert_eq!(extract(&j, "$.b[9]"), None);
        // Serialización compacta y estable.
        assert_eq!(
            to_string(&extract(&j, "$.b[2]").unwrap().clone()),
            r#"{"c":"hi"}"#
        );
    }

    #[test]
    fn rejects_invalid() {
        assert!(parse("{bad}").is_none());
        assert!(parse("[1, 2,]").is_none());
        assert!(parse("123 456").is_none());
        assert!(parse(r#"{"a":1"#).is_none());
        assert!(parse("nul").is_none());
    }

    #[test]
    fn numbers_and_escapes() {
        assert_eq!(parse("-12"), Some(Json::Int(-12)));
        assert_eq!(parse("3.5"), Some(Json::Float(3.5)));
        assert_eq!(parse("1e3"), Some(Json::Float(1000.0)));
        assert_eq!(parse(r#""a\"b\n""#), Some(Json::Str("a\"b\n".into())));
        assert_eq!(parse(r#""A""#), Some(Json::Str("A".into())));
    }
}
