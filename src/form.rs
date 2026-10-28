//! Clojure forms: an AST shared by the generator and the reducer.
//!
//! The generator builds [`Form`] values programmatically and prints them;
//! the reducer parses program text into [`Form`]s, manipulates them, and
//! prints them back. The parser covers the full syntax the generator emits
//! plus the common plain-Clojure subset (comments, quoting, deref, metadata,
//! chars, regex literals); exotic reader features (syntax-quote, tagged
//! literals, `#(...)` fns, ratios, bigints) are rejected with a clear error.

/// A Clojure form.
#[derive(Clone, Debug, PartialEq)]
pub enum Form {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Char(char),
    /// Keyword without the leading colon (`"k0"`, `"a/b"`).
    Keyword(String),
    Symbol(String),
    /// Regex pattern source (`#"..."` contents, escapes interpreted).
    Regex(String),
    List(Vec<Form>),
    Vector(Vec<Form>),
    Map(Vec<(Form, Form)>),
    Set(Vec<Form>),
    Quote(Box<Form>),
    Deref(Box<Form>),
    Meta(Box<Form>, Box<Form>),
}

impl Form {
    pub fn nil() -> Form {
        Form::Nil
    }
    pub fn boolean(b: bool) -> Form {
        Form::Bool(b)
    }
    pub fn int(i: i64) -> Form {
        Form::Int(i)
    }
    pub fn float(f: f64) -> Form {
        Form::Float(f)
    }
    pub fn string(s: impl Into<String>) -> Form {
        Form::Str(s.into())
    }
    pub fn char_(c: char) -> Form {
        Form::Char(c)
    }
    pub fn kw(s: impl Into<String>) -> Form {
        Form::Keyword(s.into())
    }
    pub fn sym(s: impl Into<String>) -> Form {
        Form::Symbol(s.into())
    }
    /// `(name args...)`.
    pub fn call(name: &str, args: Vec<Form>) -> Form {
        let mut v = Vec::with_capacity(args.len() + 1);
        v.push(Form::Symbol(name.to_string()));
        v.extend(args);
        Form::List(v)
    }
    pub fn list(forms: Vec<Form>) -> Form {
        Form::List(forms)
    }
    pub fn vector(forms: Vec<Form>) -> Form {
        Form::Vector(forms)
    }
    pub fn map(pairs: Vec<(Form, Form)>) -> Form {
        Form::Map(pairs)
    }

    /// Number of nodes in this form (including itself).
    pub fn size(&self) -> usize {
        match self {
            Form::Nil
            | Form::Bool(_)
            | Form::Int(_)
            | Form::Float(_)
            | Form::Str(_)
            | Form::Char(_)
            | Form::Keyword(_)
            | Form::Symbol(_)
            | Form::Regex(_) => 1,
            Form::List(fs) | Form::Vector(fs) | Form::Set(fs) => {
                1 + fs.iter().map(Form::size).sum::<usize>()
            }
            Form::Map(ps) => 1 + ps.iter().map(|(k, v)| k.size() + v.size()).sum::<usize>(),
            Form::Quote(f) | Form::Deref(f) => 1 + f.size(),
            Form::Meta(m, f) => 1 + m.size() + f.size(),
        }
    }

    /// If this is a list headed by a symbol, return that symbol.
    pub fn head_symbol(&self) -> Option<&str> {
        match self {
            Form::List(fs) => match fs.first() {
                Some(Form::Symbol(s)) => Some(s.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Immediate child forms (empty for atoms).
    pub fn children(&self) -> Vec<&Form> {
        match self {
            Form::List(fs) | Form::Vector(fs) | Form::Set(fs) => fs.iter().collect(),
            Form::Map(ps) => ps.iter().flat_map(|(k, v)| [k, v]).collect(),
            Form::Quote(x) | Form::Deref(x) => vec![x],
            Form::Meta(m, x) => vec![m, x],
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

/// Format a double so it always reads back as a Clojure double literal.
/// Rust's `{}` prints `1.0` as `1`, which Clojure would read as a long.
pub fn format_double(v: f64) -> String {
    if v.is_nan() {
        return "##NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 {
            "##Inf".to_string()
        } else {
            "##-Inf".to_string()
        };
    }
    // Rust's Display for floats never uses exponents, so huge/tiny values
    // would print as hundreds of digits; switch those to scientific form.
    let plain = format!("{v}");
    if plain.len() > 24 {
        format!("{:e}", v)
    } else if plain.contains(['.', 'e', 'E']) {
        plain
    } else {
        format!("{plain}.0")
    }
}

fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

fn print_atom(f: &Form, out: &mut String) {
    match f {
        Form::Nil => out.push_str("nil"),
        Form::Bool(true) => out.push_str("true"),
        Form::Bool(false) => out.push_str("false"),
        Form::Int(i) => out.push_str(&i.to_string()),
        Form::Float(f) => out.push_str(&format_double(*f)),
        Form::Str(s) => {
            out.push('"');
            escape_into(out, s);
            out.push('"');
        }
        Form::Char('\n') => out.push_str("\\newline"),
        Form::Char(' ') => out.push_str("\\space"),
        Form::Char('\t') => out.push_str("\\tab"),
        Form::Char('\r') => out.push_str("\\return"),
        Form::Char('\u{8}') => out.push_str("\\backspace"),
        Form::Char('\u{c}') => out.push_str("\\formfeed"),
        Form::Char(c) => {
            out.push('\\');
            out.push(*c);
        }
        Form::Keyword(k) => {
            out.push(':');
            out.push_str(k);
        }
        Form::Symbol(s) => out.push_str(s),
        Form::Regex(r) => {
            out.push_str("#\"");
            escape_into(out, r);
            out.push('"');
        }
        _ => unreachable!("not an atom"),
    }
}

/// Print a form on a single line.
pub fn print_compact(f: &Form) -> String {
    let mut out = String::new();
    print_compact_into(f, &mut out);
    out
}

fn print_compact_into(f: &Form, out: &mut String) {
    match f {
        Form::List(fs) => {
            out.push('(');
            for (i, x) in fs.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                print_compact_into(x, out);
            }
            out.push(')');
        }
        Form::Vector(fs) => {
            out.push('[');
            for (i, x) in fs.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                print_compact_into(x, out);
            }
            out.push(']');
        }
        Form::Map(ps) => {
            out.push('{');
            for (i, (k, v)) in ps.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                print_compact_into(k, out);
                out.push(' ');
                print_compact_into(v, out);
            }
            out.push('}');
        }
        Form::Set(fs) => {
            out.push_str("#{");
            for (i, x) in fs.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                print_compact_into(x, out);
            }
            out.push('}');
        }
        Form::Quote(x) => {
            out.push('\'');
            print_compact_into(x, out);
        }
        Form::Deref(x) => {
            out.push('@');
            print_compact_into(x, out);
        }
        Form::Meta(m, x) => {
            out.push('^');
            print_compact_into(m, out);
            out.push(' ');
            print_compact_into(x, out);
        }
        atom => print_atom(atom, out),
    }
}

/// Target line width for pretty printing.
pub const PRETTY_WIDTH: usize = 100;

/// Block-structured list heads: the first `n` elements stay on the opening
/// line and the rest are treated as body forms, one per line.
const BLOCK_HEADS: &[(&str, usize)] = &[
    ("defn", 3),
    ("defn-", 3),
    ("def", 2),
    ("defonce", 2),
    ("let", 2),
    ("loop", 2),
    ("if", 1),
    ("if-not", 1),
    ("when", 2),
    ("when-not", 2),
    ("do", 1),
    ("fn", 2),
    ("try", 1),
    ("catch", 3),
    ("finally", 1),
    ("cond", 1),
    ("case", 2),
    ("dotimes", 2),
    ("doseq", 2),
    ("for", 2),
    ("ns", 1),
    ("defrecord", 3),
    ("deftype", 3),
    ("comment", 1),
    ("->", 2),
    ("->>", 2),
];

/// Pretty-print one top-level form.
pub fn print_pretty(f: &Form) -> String {
    pretty(f, 0)
}

fn indent(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push(' ');
    }
}

fn pretty(f: &Form, col: usize) -> String {
    let one = print_compact(f);
    if col + one.len() <= PRETTY_WIDTH {
        return one;
    }
    match f {
        Form::List(fs) => {
            if fs.is_empty() {
                return "()".to_string();
            }
            let head_count = match &fs[0] {
                Form::Symbol(s) => BLOCK_HEADS
                    .iter()
                    .find(|(h, _)| *h == s.as_str())
                    .map(|(_, n)| *n),
                _ => None,
            };
            let mut out = String::from("(");
            match head_count {
                Some(n) if fs.len() > n => {
                    // Head elements on the first line, body one per line.
                    for (i, x) in fs.iter().take(n).enumerate() {
                        if i > 0 {
                            out.push(' ');
                        }
                        out.push_str(&pretty(x, col + out.len()));
                    }
                    for x in fs.iter().skip(n) {
                        out.push('\n');
                        indent(&mut out, col + 2);
                        out.push_str(&pretty(x, col + 2));
                    }
                    out.push(')');
                    out
                }
                _ => {
                    // Generic: every element on its own line.
                    for (i, x) in fs.iter().enumerate() {
                        if i == 0 {
                            out.push_str(&pretty(x, col + 1));
                        } else {
                            out.push('\n');
                            indent(&mut out, col + 1);
                            out.push_str(&pretty(x, col + 1));
                        }
                    }
                    out.push(')');
                    out
                }
            }
        }
        Form::Vector(fs) => {
            let mut out = String::from("[");
            for (i, x) in fs.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                    indent(&mut out, col + 1);
                }
                out.push_str(&pretty(x, col + 1));
            }
            out.push(']');
            out
        }
        Form::Map(ps) => {
            let mut out = String::from("{");
            for (i, (k, v)) in ps.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                    indent(&mut out, col + 1);
                }
                out.push_str(&pretty(k, col + 1));
                out.push(' ');
                let printed_k = pretty(k, col + 1);
                out.push_str(&pretty(v, col + 1 + printed_k.len() + 1));
            }
            out.push('}');
            out
        }
        Form::Set(fs) => {
            let mut out = String::from("#{");
            for (i, x) in fs.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                    indent(&mut out, col + 2);
                }
                out.push_str(&pretty(x, col + 2));
            }
            out.push('}');
            out
        }
        Form::Quote(x) => format!("'{}", pretty(x, col + 1)),
        Form::Deref(x) => format!("@{}", pretty(x, col + 1)),
        Form::Meta(m, x) => {
            let pm = pretty(m, col + 1);
            format!("^{pm} {}", pretty(x, col + 2 + pm.len()))
        }
        _ => one,
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A parse failure with a byte offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse error at offset {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for ParseError {}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            offset: self.pos,
            message: message.into(),
        })
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    /// Skip whitespace, commas, comments, and `#_` discards.
    fn skip_trivia(&mut self) -> Result<(), ParseError> {
        loop {
            // Whitespace and commas.
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r' | b',')) {
                self.pos += 1;
            }
            match self.peek() {
                Some(b';') => {
                    // Line comment.
                    while !matches!(self.peek(), None | Some(b'\n')) {
                        self.pos += 1;
                    }
                }
                Some(b'#') if self.bytes.get(self.pos + 1) == Some(&b'_') => {
                    // Discard next form.
                    self.pos += 2;
                    self.skip_trivia()?;
                    if self.peek().is_none() {
                        return self.err("expected a form after #_");
                    }
                    self.read_form()?;
                }
                _ => return Ok(()),
            }
        }
    }

    fn read_forms_until(&mut self, end: u8, what: &str) -> Result<Vec<Form>, ParseError> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                None => return self.err(format!("unterminated {what}")),
                Some(b) if b == end => {
                    self.pos += 1;
                    return Ok(out);
                }
                _ => out.push(self.read_form()?),
            }
        }
    }

    fn read_string(&mut self) -> Result<String, ParseError> {
        // Opening quote already consumed.
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return self.err("unterminated string"),
                Some(b'"') => return Ok(out),
                Some(b'\\') => match self.bump() {
                    None => return self.err("unterminated escape"),
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'b') => out.push('\u{8}'),
                    Some(b'f') => out.push('\u{c}'),
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'u') => {
                        if self.pos + 4 > self.bytes.len() {
                            return self.err("incomplete unicode escape");
                        }
                        let hex = &self.bytes[self.pos..self.pos + 4];
                        let hex = std::str::from_utf8(hex).map_err(|_| ParseError {
                            offset: self.pos,
                            message: "invalid unicode escape".to_string(),
                        })?;
                        let code = u32::from_str_radix(hex, 16).map_err(|_| ParseError {
                            offset: self.pos,
                            message: "invalid unicode escape".to_string(),
                        })?;
                        let c = char::from_u32(code).ok_or_else(|| ParseError {
                            offset: self.pos,
                            message: "invalid unicode escape".to_string(),
                        })?;
                        out.push(c);
                        self.pos += 4;
                    }
                    Some(other) => {
                        return self.err(format!("unsupported escape: \\{}", other as char));
                    }
                },
                Some(_) => {
                    // Raw bytes (possibly multi-byte UTF-8); decode one char.
                    let rest = &self.bytes[self.pos - 1..];
                    let s = std::str::from_utf8(rest).map_err(|_| ParseError {
                        offset: self.pos - 1,
                        message: "invalid UTF-8".to_string(),
                    })?;
                    let c = s.chars().next().unwrap();
                    out.push(c);
                    self.pos += c.len_utf8() - 1;
                }
            }
        }
    }

    fn is_token_char(b: u8) -> bool {
        !matches!(
            b,
            b' ' | b'\t'
                | b'\n'
                | b'\r'
                | b','
                | b'('
                | b')'
                | b'['
                | b']'
                | b'{'
                | b'}'
                | b'"'
                | b';'
                | b'\''
                | b'@'
                | b'`'
                | b'~'
                | b'^'
        )
    }

    fn read_token(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if !Self::is_token_char(b) {
                break;
            }
            self.pos += 1;
        }
        // Tokens are ASCII-heavy; fall back losslessly (input is valid UTF-8
        // and token chars exclude multi-byte lead bytes only if...). Since
        // any non-delimiter byte continues the token, slicing at byte
        // boundaries is safe: delimiters are all ASCII.
        String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned()
    }

    fn read_char(&mut self) -> Result<Form, ParseError> {
        // Backslash already consumed.
        let token = self.read_token();
        let c = match token.as_str() {
            "newline" => '\n',
            "space" => ' ',
            "tab" => '\t',
            "return" => '\r',
            "backspace" => '\u{8}',
            "formfeed" => '\u{c}',
            t if t.starts_with('u') && t.len() == 5 => {
                let code =
                    u32::from_str_radix(&t[1..], 16).map_err(|_| "invalid unicode char literal");
                match code.and_then(|c| char::from_u32(c).ok_or("invalid unicode char literal")) {
                    Ok(c) => c,
                    Err(e) => return self.err(e),
                }
            }
            t => {
                let mut chars = t.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => c,
                    _ => return self.err(format!("invalid char literal: \\{t}")),
                }
            }
        };
        Ok(Form::Char(c))
    }

    fn read_dispatch(&mut self) -> Result<Form, ParseError> {
        // `#` already consumed.
        match self.bump() {
            Some(b'"') => Ok(Form::Regex(self.read_string()?)),
            Some(b'{') => Ok(Form::Set(self.read_forms_until(b'}', "set")?)),
            Some(b'#') => {
                let mut token = String::from("##");
                token.push_str(&self.read_token());
                match token.as_str() {
                    "##Inf" => Ok(Form::Float(f64::INFINITY)),
                    "##-Inf" => Ok(Form::Float(f64::NEG_INFINITY)),
                    "##NaN" => Ok(Form::Float(f64::NAN)),
                    _ => self.err(format!("unsupported literal: {token}")),
                }
            }
            Some(b'\'') => self.err("var literals (#'...) are not supported"),
            Some(b'(') => self.err("anonymous fn literals (#(...)) are not supported"),
            Some(b'?') => self.err("reader conditionals are not supported"),
            Some(b'_') => self.err("internal error: stray #_"),
            Some(b':') => self.err("namespaced maps (#:...) are not supported"),
            Some(other) if Self::is_token_char(other) => {
                let mut token = String::from("#");
                token.push(other as char);
                token.push_str(&self.read_token());
                self.err(format!("tagged literals ({token}) are not supported"))
            }
            _ => self.err("unexpected character after #"),
        }
    }

    fn read_number_or_symbol(&mut self, first: u8) -> Result<Form, ParseError> {
        let mut token = String::from(first as char);
        token.push_str(&self.read_token());
        match token.as_str() {
            "nil" => return Ok(Form::Nil),
            "true" => return Ok(Form::Bool(true)),
            "false" => return Ok(Form::Bool(false)),
            _ => {}
        }
        if looks_like_number(&token) {
            if token.contains('/') {
                return self.err(format!("ratios ({token}) are not supported"));
            }
            if token.ends_with(['N', 'M']) {
                return self.err(format!("big numbers ({token}) are not supported"));
            }
            if token.contains(['.', 'e', 'E']) {
                match token.parse::<f64>() {
                    Ok(f) => return Ok(Form::Float(f)),
                    Err(_) => return self.err(format!("invalid number: {token}")),
                }
            }
            // Integer (possibly with a sign and/or radix prefix).
            let (neg, rest) = match token.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, token.strip_prefix('+').unwrap_or(token.as_str())),
            };
            let (radix, digits) = if let Some(h) =
                rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X"))
            {
                (16, h)
            } else if let Some(h) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
                (2, h)
            } else if let Some(h) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
                (8, h)
            } else {
                (10, rest)
            };
            // Plain leading-zero octal (Clojure reads 017 as 15).
            let (radix, digits) = if radix == 10
                && digits.len() > 1
                && digits.starts_with('0')
                && digits.bytes().all(|b| b.is_ascii_digit())
            {
                (8, digits)
            } else {
                (radix, digits)
            };
            // Parse the magnitude as u64 so that i64::MIN (-9223372036854775808,
            // whose magnitude overflows i64) is accepted.
            match u64::from_str_radix(digits, radix) {
                Ok(mag) => {
                    if neg {
                        if mag > i64::MAX as u64 + 1 {
                            return self.err(format!("invalid number: {token}"));
                        }
                        return Ok(Form::Int((mag as i64).wrapping_neg()));
                    } else {
                        if mag > i64::MAX as u64 {
                            return self.err(format!("invalid number: {token}"));
                        }
                        return Ok(Form::Int(mag as i64));
                    }
                }
                Err(_) => return self.err(format!("invalid number: {token}")),
            }
        }
        Ok(Form::Symbol(token))
    }

    fn read_form(&mut self) -> Result<Form, ParseError> {
        self.skip_trivia()?;
        let b = match self.bump() {
            Some(b) => b,
            None => return self.err("unexpected end of input"),
        };
        match b {
            b'(' => Ok(Form::List(self.read_forms_until(b')', "list")?)),
            b'[' => Ok(Form::Vector(self.read_forms_until(b']', "vector")?)),
            b'{' => {
                let forms = self.read_forms_until(b'}', "map")?;
                if forms.len() % 2 != 0 {
                    return self.err("map literal must contain an even number of forms");
                }
                let mut pairs = Vec::with_capacity(forms.len() / 2);
                let mut it = forms.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    pairs.push((k, v));
                }
                Ok(Form::Map(pairs))
            }
            b'\'' => Ok(Form::Quote(Box::new(self.read_form()?))),
            b'@' => Ok(Form::Deref(Box::new(self.read_form()?))),
            b'`' => self.err("syntax-quote is not supported"),
            b'~' => self.err("unquote is not supported"),
            b'^' => {
                let m = self.read_form()?;
                let f = self.read_form()?;
                Ok(Form::Meta(Box::new(m), Box::new(f)))
            }
            b'"' => Ok(Form::Str(self.read_string()?)),
            b'\\' => self.read_char(),
            b'#' => self.read_dispatch(),
            b':' => {
                let token = self.read_token();
                if token.is_empty() {
                    return self.err("empty keyword");
                }
                Ok(Form::Keyword(token))
            }
            b')' | b']' | b'}' => self.err("unmatched closing delimiter"),
            b => self.read_number_or_symbol(b),
        }
    }
}

fn looks_like_number(token: &str) -> bool {
    let t = token.strip_prefix(['-', '+']).unwrap_or(token);
    let mut chars = t.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() => true,
        // Leading-dot floats such as ".5" or "-.5".
        Some('.') => chars.next().map(|c| c.is_ascii_digit()).unwrap_or(false),
        _ => false,
    }
}

/// Parse every top-level form in `input`.
pub fn parse_all(input: &str) -> Result<Vec<Form>, ParseError> {
    let mut p = Parser::new(input);
    let mut out = Vec::new();
    loop {
        p.skip_trivia()?;
        if p.peek().is_none() {
            return Ok(out);
        }
        out.push(p.read_form()?);
    }
}

/// Parse exactly one form; trailing content is an error.
pub fn parse_one(input: &str) -> Result<Form, ParseError> {
    let mut p = Parser::new(input);
    p.skip_trivia()?;
    let f = p.read_form()?;
    p.skip_trivia()?;
    if p.peek().is_some() {
        return Err(ParseError {
            offset: p.pos,
            message: "unexpected trailing content".to_string(),
        });
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(src: &str) {
        let forms = parse_all(src).unwrap_or_else(|e| panic!("parse failed for {src:?}: {e}"));
        assert!(!forms.is_empty());
        let printed: Vec<String> = forms.iter().map(print_compact).collect();
        let again = parse_all(&printed.join("\n")).expect("reparse failed");
        assert_eq!(forms, again, "round-trip mismatch for {src:?}");
    }

    #[test]
    fn atoms() {
        assert_eq!(parse_one("nil").unwrap(), Form::Nil);
        assert_eq!(parse_one("true").unwrap(), Form::Bool(true));
        assert_eq!(parse_one("42").unwrap(), Form::Int(42));
        assert_eq!(parse_one("-17").unwrap(), Form::Int(-17));
        assert_eq!(
            parse_one("-9223372036854775808").unwrap(),
            Form::Int(i64::MIN)
        );
        assert_eq!(
            parse_one("9223372036854775807").unwrap(),
            Form::Int(i64::MAX)
        );
        assert!(parse_one("9223372036854775808").is_err());
        assert!(parse_one("-9223372036854775809").is_err());
        assert_eq!(parse_one("0x10").unwrap(), Form::Int(16));
        assert_eq!(parse_one("017").unwrap(), Form::Int(15));
        assert_eq!(parse_one("0b101").unwrap(), Form::Int(5));
        assert_eq!(parse_one("1.5").unwrap(), Form::Float(1.5));
        assert_eq!(parse_one("1e3").unwrap(), Form::Float(1000.0));
        assert_eq!(parse_one("##Inf").unwrap(), Form::Float(f64::INFINITY));
        assert_eq!(
            parse_one("\"a\\nb\"").unwrap(),
            Form::Str("a\nb".to_string())
        );
        assert_eq!(parse_one(":k0").unwrap(), Form::Keyword("k0".to_string()));
        assert_eq!(
            parse_one("foo/bar").unwrap(),
            Form::Symbol("foo/bar".to_string())
        );
        assert_eq!(parse_one("\\a").unwrap(), Form::Char('a'));
        assert_eq!(parse_one("\\newline").unwrap(), Form::Char('\n'));
        assert_eq!(parse_one("#\"a+\"").unwrap(), Form::Regex("a+".to_string()));
    }

    #[test]
    fn collections_and_reader_macros() {
        assert_eq!(
            parse_one("(a 1 [2 {:k v}])").unwrap(),
            Form::list(vec![
                Form::sym("a"),
                Form::int(1),
                Form::vector(vec![
                    Form::int(2),
                    Form::map(vec![(Form::kw("k"), Form::sym("v"))])
                ])
            ])
        );
        assert_eq!(
            parse_one("'x").unwrap(),
            Form::Quote(Box::new(Form::sym("x")))
        );
        assert_eq!(
            parse_one("@g0").unwrap(),
            Form::Deref(Box::new(Form::sym("g0")))
        );
        assert_eq!(
            parse_one("^String x").unwrap(),
            Form::Meta(Box::new(Form::sym("String")), Box::new(Form::sym("x")))
        );
        assert_eq!(
            parse_one("#{1 2}").unwrap(),
            Form::Set(vec![Form::int(1), Form::int(2)])
        );
    }

    #[test]
    fn trivia() {
        let forms = parse_all(";; comment\n(a 1) ,,, ; another\n#_(skipped [1 2]) [3]").unwrap();
        assert_eq!(forms.len(), 2);
        assert_eq!(forms[0].head_symbol(), Some("a"));
    }

    #[test]
    fn errors() {
        assert!(parse_one("(a").is_err());
        assert!(parse_one("[1 2}").is_err());
        assert!(parse_one("{:a}").is_err());
        assert!(parse_one("\"abc").is_err());
        assert!(parse_one(")").is_err());
        assert!(parse_one("1/3").is_err());
        assert!(parse_one("1N").is_err());
        assert!(parse_one("#inst \"2020\"").is_err());
        assert!(parse_one("`x").is_err());
        assert!(parse_all("(a) (b)").unwrap().len() == 2);
        assert!(parse_one("(a) (b)").is_err());
    }

    #[test]
    fn roundtrips() {
        roundtrip("(defn f [x] (+ x 1))");
        roundtrip("{:a [1 2] :b {:c \"s\\n\"}}");
        roundtrip("(ns fuzz-1 (:require [clojure.string :as str]))");
        roundtrip("@g0");
        roundtrip("(s-subs s 0 (count s))");
        roundtrip("[1.5 ##Inf ##-Inf -0.5 100.0]");
        roundtrip("(println \"checksum:\" (smith-hash [@g0 r0]))");
    }

    #[test]
    fn double_formatting() {
        assert_eq!(format_double(1.0), "1.0");
        assert_eq!(format_double(-1.0), "-1.0");
        assert_eq!(format_double(0.0), "0.0");
        assert_eq!(format_double(1.5), "1.5");
        assert_eq!(format_double(1e300), "1e300");
        assert_eq!(format_double(f64::INFINITY), "##Inf");
        assert_eq!(format_double(f64::NEG_INFINITY), "##-Inf");
        assert_eq!(format_double(f64::NAN), "##NaN");
        // Everything produced must re-parse as a float.
        for v in [
            0.0,
            1.0,
            -42.0,
            1e21,
            1e-7,
            123456.789,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            match parse_one(&format_double(v)).unwrap() {
                Form::Float(f) => assert_eq!(f, v, "mismatch for {v}"),
                other => panic!("{v} printed as non-float: {other:?}"),
            }
        }
    }

    #[test]
    fn pretty_printing_basics() {
        let short = parse_one("(def x 1)").unwrap();
        assert_eq!(print_pretty(&short), "(def x 1)");
        let long = parse_one("(defn some-function-name [argument-one argument-two] (+ argument-one argument-two) (* argument-one argument-two) (- argument-one argument-two))").unwrap();
        let pretty = print_pretty(&long);
        assert!(
            pretty.contains('\n'),
            "expected multi-line output:\n{pretty}"
        );
        // Pretty output must re-parse to the same form.
        assert_eq!(parse_one(&pretty).unwrap(), long);
    }
}
