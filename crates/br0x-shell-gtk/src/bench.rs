//! Script-driven testing hook for br0x. Not wired to the UI yet.
//!
//! A JSON-lines protocol over a Unix socket in the XDG state dir. The shell
//! crate has no serde_json (only br0x-core does, and this module must stay
//! dependency-free), so parsing and serialization below are hand-rolled for
//! the small object shapes the protocol needs. See BENCH_WIRING.md for the
//! lines that start the server from main.rs.
//!
//! Bench tabs are isolated by contract: `open` must create a tab that never
//! enters the session file, history, or suggestions. The server only speaks
//! the protocol; the shell wires real WebViews by implementing BenchHandler.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Env flag that enables the server. Off unless explicitly set.
pub const ENABLE_ENV: &str = "BR0X_BENCH";
/// Socket file name under `$XDG_STATE_HOME/br0x/`.
pub const SOCKET_NAME: &str = "bench.sock";

/// True only when the explicit opt-in flag is set. The server never listens
/// by default; a stray socket must not appear on user machines.
pub fn is_enabled() -> bool {
    is_enabled_flag(std::env::var_os(ENABLE_ENV).as_ref().and_then(|v| v.to_str()))
}

fn is_enabled_flag(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true"))
}

/// Socket path, honouring `$XDG_STATE_HOME` with the same fallbacks the
/// shell uses elsewhere (`~/.local/state`, then /tmp).
pub fn socket_path() -> PathBuf {
    socket_path_from(
        std::env::var_os("XDG_STATE_HOME").as_ref().and_then(|v| v.to_str()),
        std::env::var_os("HOME").as_ref().and_then(|v| v.to_str()),
    )
}

fn socket_path_from(state_home: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(dir) = state_home.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("br0x").join(SOCKET_NAME);
    }
    match home.filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h).join(".local/state/br0x").join(SOCKET_NAME),
        None => PathBuf::from("/tmp").join("br0x").join(SOCKET_NAME),
    }
}

/// Which tabs `close` acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum CloseTarget {
    One(u64),
    All,
}

/// One decoded client request.
#[derive(Debug, Clone, PartialEq)]
pub enum BenchRequest {
    Open { url: String },
    Wait { id: u64 },
    Text { id: u64 },
    Shot { id: u64, path: String },
    Click { id: u64, selector: String },
    Probe,
    Close { target: CloseTarget },
    Tabs,
}

/// One listed tab in `tabs` responses.
#[derive(Debug, Clone, PartialEq)]
pub struct TabInfo {
    pub id: u64,
    pub url: String,
    pub loaded: bool,
}

impl TabInfo {
    fn append_json(&self, out: &mut String) {
        out.push_str("{\"id\":");
        out.push_str(&self.id.to_string());
        out.push_str(",\"url\":\"");
        escape_into(out, &self.url);
        out.push_str("\",\"loaded\":");
        out.push_str(if self.loaded { "true" } else { "false" });
        out.push('}');
    }
}

/// Window state in `probe` responses.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowState {
    pub tabs: u64,
    pub visible: bool,
    pub selected: Option<u64>,
}

impl WindowState {
    fn append_json(&self, out: &mut String) {
        out.push_str("{\"tabs\":");
        out.push_str(&self.tabs.to_string());
        out.push_str(",\"visible\":");
        out.push_str(if self.visible { "true" } else { "false" });
        out.push_str(",\"selected\":");
        match self.selected {
            Some(id) => out.push_str(&id.to_string()),
            None => out.push_str("null"),
        }
        out.push('}');
    }
}

/// Append `s` JSON-escaped (no surrounding quotes). Covers control bytes so
/// a page title can never break the response line.
pub fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

fn ok() -> String {
    "{\"ok\":true}".to_string()
}

fn ok_error(err: &str) -> String {
    let mut out = String::from("{\"ok\":false,\"error\":\"");
    escape_into(&mut out, err);
    out.push_str("\"}");
    out
}

fn ok_id(id: u64) -> String {
    format!("{{\"ok\":true,\"id\":{id}}}")
}

fn ok_text(text: &str) -> String {
    let mut out = String::from("{\"ok\":true,\"text\":\"");
    escape_into(&mut out, text);
    out.push_str("\"}");
    out
}

fn ok_tabs(tabs: &[TabInfo]) -> String {
    let mut out = String::from("{\"ok\":true,\"tabs\":[");
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        tab.append_json(&mut out);
    }
    out.push_str("]}");
    out
}

fn ok_probe(state: &WindowState) -> String {
    let mut out = String::from("{\"ok\":true,\"state\":");
    state.append_json(&mut out);
    out.push('}');
    out
}

// Minimal JSON values: only what the request shapes need.
#[derive(Debug, Clone, PartialEq)]
enum JsonVal {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Obj(Vec<(String, JsonVal)>),
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(line: &'a str) -> Self {
        Self { bytes: line.as_bytes(), pos: 0 }
    }

    fn parse_value(&mut self) -> Result<JsonVal, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => Ok(JsonVal::Str(self.parse_string()?)),
            Some(b'{') => self.parse_object(),
            Some(b't') => self.parse_lit("true", JsonVal::Bool(true)),
            Some(b'f') => self.parse_lit("false", JsonVal::Bool(false)),
            Some(b'n') => self.parse_lit("null", JsonVal::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => Ok(JsonVal::Num(self.parse_number()?)),
            Some(c) => Err(format!("unexpected byte '{c}'")),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn parse_object(&mut self) -> Result<JsonVal, String> {
        self.pos += 1; // consume '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonVal::Obj(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("object keys must be strings".to_string());
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err("object key without ':'".to_string());
            }
            self.pos += 1;
            let value = self.parse_value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonVal::Obj(fields));
                }
                _ => return Err("expected ',' or '}'".to_string()),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.pos += 1; // consume opening quote
        let mut out = String::new();
        loop {
            let Some(&b) = self.bytes.get(self.pos) else {
                return Err("unterminated string".to_string());
            };
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&e) = self.bytes.get(self.pos) else {
                        return Err("unterminated escape".to_string());
                    };
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'u' => out.push(self.parse_unicode()?),
                        _ => return Err("bad escape".to_string()),
                    }
                }
                0x00..=0x1F => return Err("raw control in string".to_string()),
                _ => {
                    // Multi-byte UTF-8: consume the full sequence at once.
                    let rest = &self.bytes[self.pos - 1..];
                    let s = std::str::from_utf8(rest).map_err(|_| "bad utf-8".to_string())?;
                    let c = s.chars().next().ok_or("bad utf-8".to_string())?;
                    out.push(c);
                    self.pos += c.len_utf8() - 1;
                }
            }
        }
    }

    fn parse_unicode(&mut self) -> Result<char, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("bad \\u escape".to_string());
        }
        let digits = &self.bytes[self.pos..self.pos + 4];
        let text = std::str::from_utf8(digits).map_err(|_| "bad \\u escape".to_string())?;
        let code = u32::from_str_radix(text, 16).map_err(|_| "bad \\u escape".to_string())?;
        self.pos += 4;
        char::from_u32(code).ok_or("bad \\u escape".to_string())
    }

    fn parse_lit(&mut self, lit: &str, value: JsonVal) -> Result<JsonVal, String> {
        if self.bytes.get(self.pos..self.pos + lit.len()) == Some(lit.as_bytes()) {
            self.pos += lit.len();
            Ok(value)
        } else {
            Err(format!("expected '{lit}'"))
        }
    }

    fn parse_number(&mut self) -> Result<String, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.bytes.get(start..self.pos).is_none_or(|s| s.is_empty() || s == b"-") {
            return Err("bad number".to_string());
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned())
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
}

fn get_field<'a>(obj: &'a [(String, JsonVal)], name: &str) -> Option<&'a JsonVal> {
    obj.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn need_str(obj: &[(String, JsonVal)], name: &str) -> Result<String, String> {
    match get_field(obj, name) {
        Some(JsonVal::Str(s)) => Ok(s.clone()),
        Some(_) => Err(format!("\"{name}\" must be a string")),
        None => Err(format!("missing \"{name}\"")),
    }
}

fn need_id(obj: &[(String, JsonVal)]) -> Result<u64, String> {
    match get_field(obj, "id") {
        Some(JsonVal::Num(raw)) => {
            raw.parse::<u64>().map_err(|_| "\"id\" must fit u64".to_string())
        }
        Some(_) => Err("\"id\" must be a number".to_string()),
        None => Err("missing \"id\"".to_string()),
    }
}

/// Decode one protocol line into a request. Anything malformed yields a
/// human-readable error the server sends back as `{"ok":false,...}`.
pub fn parse_request(line: &str) -> Result<BenchRequest, String> {
    let mut parser = Parser::new(line);
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err("trailing bytes after object".to_string());
    }
    let JsonVal::Obj(obj) = value else {
        return Err("request must be an object".to_string());
    };
    let cmd = need_str(&obj, "cmd")?;
    match cmd.as_str() {
        "open" => Ok(BenchRequest::Open { url: need_str(&obj, "url")? }),
        "wait" => Ok(BenchRequest::Wait { id: need_id(&obj)? }),
        "text" => Ok(BenchRequest::Text { id: need_id(&obj)? }),
        "shot" => Ok(BenchRequest::Shot { id: need_id(&obj)?, path: need_str(&obj, "path")? }),
        "click" => {
            Ok(BenchRequest::Click { id: need_id(&obj)?, selector: need_str(&obj, "selector")? })
        }
        "probe" => Ok(BenchRequest::Probe),
        "tabs" => Ok(BenchRequest::Tabs),
        "close" => match get_field(&obj, "target") {
            Some(JsonVal::Str(s)) if s == "all" => {
                Ok(BenchRequest::Close { target: CloseTarget::All })
            }
            Some(JsonVal::Num(raw)) => raw
                .parse::<u64>()
                .map(CloseTarget::One)
                .map(|target| BenchRequest::Close { target })
                .map_err(|_| "\"target\" must be an id or \"all\"".to_string()),
            Some(_) => Err("\"target\" must be an id or \"all\"".to_string()),
            None => Err("missing \"target\"".to_string()),
        },
        other => Err(format!("unknown cmd \"{other}\"")),
    }
}

/// The shell side of the protocol. The integrator implements this over real
/// WebViews; every method runs on a socket thread, so a GTK implementation
/// must hop to the main thread (e.g. via a glib channel) before touching a
/// view. Bench tabs stay out of the session, history, and suggestions.
pub trait BenchHandler: Send + 'static {
    /// Open `url` in an isolated bench tab and return its id.
    fn open(&mut self, url: &str) -> Result<u64, String>;
    /// Block until the tab reports loaded.
    fn wait(&mut self, id: u64) -> Result<(), String>;
    /// Visible text of the tab.
    fn text(&mut self, id: u64) -> Result<String, String>;
    /// Best effort PNG snapshot via the WebKit snapshot API. Returns an
    /// error when snapshots are unavailable in this build.
    fn shot(&mut self, id: u64, path: &str) -> Result<(), String>;
    /// Dispatch a click on a CSS selector in the tab.
    fn click(&mut self, id: u64, selector: &str) -> Result<(), String>;
    /// Window state for assertions (tab count, visibility, selection).
    fn probe(&mut self) -> WindowState;
    /// Close one bench tab, or every bench tab. Never touches user tabs.
    fn close(&mut self, target: CloseTarget) -> Result<(), String>;
    /// List open bench tabs only.
    fn tabs(&mut self) -> Vec<TabInfo>;
}

/// Run one request against a handler and serialize the response line.
pub fn dispatch<H: BenchHandler>(handler: &mut H, req: BenchRequest) -> String {
    match req {
        BenchRequest::Open { url } => {
            handler.open(&url).map(ok_id).unwrap_or_else(|e| ok_error(&e))
        }
        BenchRequest::Wait { id } => {
            handler.wait(id).map(|()| ok()).unwrap_or_else(|e| ok_error(&e))
        }
        BenchRequest::Text { id } => {
            handler.text(id).map(|t| ok_text(&t)).unwrap_or_else(|e| ok_error(&e))
        }
        BenchRequest::Shot { id, path } => {
            handler.shot(id, &path).map(|()| ok()).unwrap_or_else(|e| ok_error(&e))
        }
        BenchRequest::Click { id, selector } => {
            handler.click(id, &selector).map(|()| ok()).unwrap_or_else(|e| ok_error(&e))
        }
        BenchRequest::Probe => ok_probe(&handler.probe()),
        BenchRequest::Close { target } => {
            handler.close(target).map(|()| ok()).unwrap_or_else(|e| ok_error(&e))
        }
        BenchRequest::Tabs => ok_tabs(&handler.tabs()),
    }
}

/// The listening socket. Owns its path; stale files are replaced on start.
pub struct BenchServer {
    path: PathBuf,
}

impl BenchServer {
    pub fn new() -> Self {
        Self { path: socket_path() }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Bind (0600, private dir) and serve `handler` on a background thread.
    /// Returns once listening; each connection is handled on its own thread.
    pub fn start<H: BenchHandler>(self, handler: H) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let _ = std::fs::remove_file(&self.path);
        let listener = UnixListener::bind(&self.path)?;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        let shared = Arc::new(Mutex::new(handler));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let shared = shared.clone();
                std::thread::spawn(move || serve_conn(stream, shared));
            }
        });
        Ok(())
    }
}

impl Default for BenchServer {
    fn default() -> Self {
        Self::new()
    }
}

fn serve_conn<H: BenchHandler>(stream: UnixStream, shared: Arc<Mutex<H>>) {
    let mut lines = BufReader::new(&stream).lines();
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let response = match parse_request(&line) {
            Ok(req) => match shared.lock() {
                Ok(mut handler) => dispatch(&mut *handler, req),
                Err(_) => ok_error("handler unavailable"),
            },
            Err(e) => ok_error(&e),
        };
        let mut stream = &stream;
        if writeln!(stream, "{response}").is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub {
        next_id: u64,
        fail_shot: bool,
    }

    impl BenchHandler for Stub {
        fn open(&mut self, url: &str) -> Result<u64, String> {
            if url.is_empty() {
                return Err("empty url".to_string());
            }
            self.next_id += 1;
            Ok(self.next_id)
        }
        fn wait(&mut self, id: u64) -> Result<(), String> {
            if id == 0 || id > self.next_id {
                return Err("no such tab".to_string());
            }
            Ok(())
        }
        fn text(&mut self, id: u64) -> Result<String, String> {
            self.wait(id)?;
            Ok("hello \"world\"\nline".to_string())
        }
        fn shot(&mut self, id: u64, _path: &str) -> Result<(), String> {
            self.wait(id)?;
            if self.fail_shot {
                return Err("snapshot unavailable in this build".to_string());
            }
            Ok(())
        }
        fn click(&mut self, id: u64, selector: &str) -> Result<(), String> {
            self.wait(id)?;
            if selector.is_empty() {
                return Err("empty selector".to_string());
            }
            Ok(())
        }
        fn probe(&mut self) -> WindowState {
            WindowState { tabs: self.next_id, visible: true, selected: Some(1) }
        }
        fn close(&mut self, target: CloseTarget) -> Result<(), String> {
            match target {
                CloseTarget::All => {
                    self.next_id = 0;
                    Ok(())
                }
                CloseTarget::One(id) => self.wait(id),
            }
        }
        fn tabs(&mut self) -> Vec<TabInfo> {
            (1..=self.next_id)
                .map(|id| TabInfo { id, url: format!("https://x.example/{id}"), loaded: true })
                .collect()
        }
    }

    fn stub() -> Stub {
        Stub { next_id: 0, fail_shot: false }
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(
            parse_request(r#"{"cmd":"open","url":"https://example.com"}"#),
            Ok(BenchRequest::Open { url: "https://example.com".into() })
        );
        assert_eq!(parse_request(r#"{"cmd":"wait","id":3}"#), Ok(BenchRequest::Wait { id: 3 }));
        assert_eq!(parse_request(r#"{"cmd":"text","id":3}"#), Ok(BenchRequest::Text { id: 3 }));
        assert_eq!(
            parse_request(r#"{"cmd":"shot","id":3,"path":"/tmp/a.png"}"#),
            Ok(BenchRequest::Shot { id: 3, path: "/tmp/a.png".into() })
        );
        assert_eq!(
            parse_request(r##"{"cmd":"click","id":3,"selector":"#ok"}"##),
            Ok(BenchRequest::Click { id: 3, selector: "#ok".into() })
        );
        assert_eq!(parse_request(r#"{"cmd":"probe"}"#), Ok(BenchRequest::Probe));
        assert_eq!(parse_request(r#"{"cmd":"tabs"}"#), Ok(BenchRequest::Tabs));
        assert_eq!(
            parse_request(r#"{"cmd":"close","target":"all"}"#),
            Ok(BenchRequest::Close { target: CloseTarget::All })
        );
        assert_eq!(
            parse_request(r#"{"cmd":"close","target":7}"#),
            Ok(BenchRequest::Close { target: CloseTarget::One(7) })
        );
    }

    #[test]
    fn rejects_malformed_requests() {
        assert!(parse_request("not json").is_err());
        assert!(parse_request("[1,2]").is_err());
        assert!(parse_request(r#"{"cmd":"fly"}"#).is_err());
        assert!(parse_request(r#"{"cmd":"open"}"#).is_err());
        assert!(parse_request(r#"{"cmd":"wait","id":"3"}"#).is_err());
        assert!(parse_request(r#"{"cmd":"close","target":true}"#).is_err());
        assert!(parse_request(r#"{"cmd":"probe"} trailing"#).is_err());
        assert!(parse_request(r#"{"url":"https://example.com"}"#).is_err());
    }

    #[test]
    fn strings_decode_escapes() {
        assert_eq!(
            parse_request(r#"{"cmd":"open","url":"a\"b\\c/d"}"#),
            Ok(BenchRequest::Open { url: "a\"b\\c/d".into() })
        );
        assert_eq!(
            parse_request(r#"{"cmd":"click","id":1,"selector":"caf\u00e9"}"#),
            Ok(BenchRequest::Click { id: 1, selector: "café".into() })
        );
    }

    #[test]
    fn responses_escape_text_and_stay_on_one_line() {
        let line = ok_text("a\"b\\c\nd\te\x01f");
        assert_eq!(line, "{\"ok\":true,\"text\":\"a\\\"b\\\\c\\nd\\te\\u0001f\"}");
        assert!(!line.contains('\n') && !line.contains('\t'));
        assert_eq!(ok_id(9), "{\"ok\":true,\"id\":9}");
        assert_eq!(ok_error("bad \"x\""), "{\"ok\":false,\"error\":\"bad \\\"x\\\"\"}");
    }

    #[test]
    fn tabs_and_probe_serialize() {
        let tabs = vec![
            TabInfo { id: 1, url: "https://a.example/\"q\"".into(), loaded: true },
            TabInfo { id: 2, url: "https://b.example".into(), loaded: false },
        ];
        assert_eq!(
            ok_tabs(&tabs),
            "{\"ok\":true,\"tabs\":[{\"id\":1,\"url\":\"https://a.example/\\\"q\\\"\",\"loaded\":true},{\"id\":2,\"url\":\"https://b.example\",\"loaded\":false}]}"
        );
        let state = WindowState { tabs: 2, visible: true, selected: None };
        assert_eq!(
            ok_probe(&state),
            "{\"ok\":true,\"state\":{\"tabs\":2,\"visible\":true,\"selected\":null}}"
        );
    }

    #[test]
    fn dispatch_roundtrips_through_a_handler() {
        let mut h = stub();
        let open = dispatch(&mut h, BenchRequest::Open { url: "https://example.com".into() });
        assert_eq!(open, "{\"ok\":true,\"id\":1}");
        assert_eq!(dispatch(&mut h, BenchRequest::Wait { id: 1 }), "{\"ok\":true}");
        assert_eq!(
            dispatch(&mut h, BenchRequest::Text { id: 1 }),
            "{\"ok\":true,\"text\":\"hello \\\"world\\\"\\nline\"}"
        );
        assert_eq!(
            dispatch(&mut h, BenchRequest::Click { id: 1, selector: "#ok".into() }),
            "{\"ok\":true}"
        );
        assert_eq!(
            dispatch(&mut h, BenchRequest::Probe),
            "{\"ok\":true,\"state\":{\"tabs\":1,\"visible\":true,\"selected\":1}}"
        );
        assert_eq!(
            dispatch(&mut h, BenchRequest::Tabs),
            "{\"ok\":true,\"tabs\":[{\"id\":1,\"url\":\"https://x.example/1\",\"loaded\":true}]}"
        );
        // Unknown ids and shot failures surface as ok:false, never a panic.
        assert_eq!(
            dispatch(&mut h, BenchRequest::Text { id: 9 }),
            "{\"ok\":false,\"error\":\"no such tab\"}"
        );
        h.fail_shot = true;
        assert_eq!(
            dispatch(&mut h, BenchRequest::Shot { id: 1, path: "/tmp/a.png".into() }),
            "{\"ok\":false,\"error\":\"snapshot unavailable in this build\"}"
        );
        assert_eq!(
            dispatch(&mut h, BenchRequest::Close { target: CloseTarget::All }),
            "{\"ok\":true}"
        );
        assert_eq!(dispatch(&mut h, BenchRequest::Tabs), "{\"ok\":true,\"tabs\":[]}");
    }

    #[test]
    fn socket_path_honours_xdg_then_home_then_tmp() {
        assert_eq!(
            socket_path_from(Some("/run/user/1000"), Some("/home/u")),
            PathBuf::from("/run/user/1000/br0x/bench.sock")
        );
        assert_eq!(
            socket_path_from(None, Some("/home/u")),
            PathBuf::from("/home/u/.local/state/br0x/bench.sock")
        );
        assert_eq!(socket_path_from(Some(""), Some("")), PathBuf::from("/tmp/br0x/bench.sock"));
        assert_eq!(socket_path_from(None, None), PathBuf::from("/tmp/br0x/bench.sock"));
    }

    #[test]
    fn server_stays_off_without_the_flag() {
        assert!(is_enabled_flag(Some("1")));
        assert!(is_enabled_flag(Some("true")));
        assert!(!is_enabled_flag(None));
        assert!(!is_enabled_flag(Some("")));
        assert!(!is_enabled_flag(Some("0")));
    }
}
