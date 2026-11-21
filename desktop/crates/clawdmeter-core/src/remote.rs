//! The hub's wire format: hook records sent over HTTP from other computers.
//!
//! A computer whose Claude Code sessions should reach a device that hangs
//! off another computer (the hub) runs its hooks with `hub_url` and
//! `hub_token` set. Each hook run then sends one [`HookRecord`] to the hub
//! (`POST /v1/hook`, `Authorization: Bearer <hub_token>`), and the hub writes
//! it into the same files its own hooks write ([`LocalWrites`]), so the rest
//! of the app can't tell a remote session from a local one.
//!
//! Everything here is pure: the request and response bytes, the checks, and
//! the lines the hub writes. The listener and the sender live in the app
//! crate (`hub.rs`).
//!
//! * The parser takes a small HTTP/1.1 subset: one request per connection,
//!   `Content-Length` bodies of at most [`MAX_BODY`] bytes, no chunking.
//! * The token is compared in constant time. The source address is not
//!   trusted (a tunnel makes every request local).
//! * Session ids get the sending host as a prefix (`dev.<sid>`), except the
//!   test alert's ([`crate::attention::TEST_SESSION`]), so the hub's
//!   own sessions and every host's stay apart.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::ConfigFile;
use crate::identity::Identity;

/// The hub's default port.
pub const DEFAULT_PORT: u16 = 47811;
/// The largest body the hub reads.
pub const MAX_BODY: usize = 8 * 1024;
/// The largest request head (request line and headers) the hub reads.
pub const MAX_HEAD: usize = 4 * 1024;
/// `hub_token` must be at least this long.
pub const MIN_TOKEN_LEN: usize = 16;
/// Records per second the hub takes before answering 429.
pub const MAX_PER_SEC: u32 = 50;
/// The record format.
pub const WIRE_VERSION: u32 = 1;

/// One hook run on another computer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HookRecord {
    /// [`WIRE_VERSION`].
    pub v: u32,
    /// The sending host's label (`hub_host`, else its host name).
    pub host: String,
    /// The sender's clock (unix s). The hub stamps lines with its own.
    #[serde(default)]
    pub ts: u64,
    /// The hook event to log (`hook_event_name`), when the hook logs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// The session id as the hook got it.
    #[serde(default)]
    pub sid: String,
    /// Who the session's account is signed in as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acct: Option<Identity>,
    /// An attention alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<RemoteAlert>,
    /// The session's alert is answered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear: bool,
}

/// An alert's kind and message (the attention file's `k` and `m`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RemoteAlert {
    /// [`crate::attention::AlertKind`] as text.
    pub k: String,
    /// The message.
    pub m: String,
}

/// The hub's settings, from the config file and the environment. Both
/// sides read them: the hub listens, every other computer's hook sends.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HubConfig {
    /// Where the hub listens (`hub_listen`, e.g. `0.0.0.0:47811`); `None` = off.
    pub listen: Option<String>,
    /// The shared secret (`hub_token`, or `CLAWDMETER_HUB_TOKEN`).
    pub token: Option<String>,
    /// Where hooks send records (`hub_url`, or `CLAWDMETER_HUB`): `host:port`.
    pub url: Option<String>,
    /// This computer's label in session ids (`hub_host`, or `CLAWDMETER_HOST`).
    pub host: Option<String>,
    /// With a hub to send to, also write this computer's own files
    /// (`hub_local`, default off).
    pub local: bool,
    /// Who each dir is, for a dir whose sign-in can't be read
    /// (`hub_accounts = ~/.claude-b=me@example.com, …`).
    pub accounts: Vec<(PathBuf, Identity)>,
}

impl HubConfig {
    /// Read from `cfg`; `env` looks up an environment variable, which wins
    /// over the file.
    pub fn from_config(cfg: &ConfigFile, env: impl Fn(&str) -> Option<String>, home: &Path) -> Self {
        let get = |env_key: &str, key: &str| {
            env(env_key)
                .or_else(|| cfg.get(key).map(str::to_owned))
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let accounts = cfg
            .get("hub_accounts")
            .unwrap_or("")
            .split(',')
            .filter_map(|e| e.split_once('='))
            .map(|(d, who)| (crate::paths::expand_tilde(home, d.trim()), Identity::from_hint(who)))
            .filter(|(_, who)| !who.is_empty())
            .collect();
        HubConfig {
            listen: cfg.get("hub_listen").map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned),
            token: get("CLAWDMETER_HUB_TOKEN", "hub_token"),
            url: get("CLAWDMETER_HUB", "hub_url").map(|u| normalize_url(&u)),
            host: get("CLAWDMETER_HOST", "hub_host"),
            local: cfg.get("hub_local").is_some_and(|v| v.trim().eq_ignore_ascii_case("on")),
            accounts,
        }
    }

    /// The token, when it is long enough to use.
    pub fn valid_token(&self) -> Option<&str> {
        self.token.as_deref().filter(|t| t.len() >= MIN_TOKEN_LEN)
    }

    /// Where to send and with what token, when this computer sends to a hub.
    pub fn sender(&self) -> Option<(&str, &str)> {
        Some((self.url.as_deref()?, self.valid_token()?))
    }

    /// The fallback identity listed for `dir`.
    pub fn account_hint(&self, dir: &Path) -> Option<&Identity> {
        self.accounts.iter().find(|(d, _)| d == dir).map(|(_, who)| who)
    }
}

/// `http://host:port/` → `host:port`; no port → [`DEFAULT_PORT`].
pub fn normalize_url(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix("http://").unwrap_or(s);
    let s = s.split('/').next().unwrap_or("");
    let has_port = s.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if has_port {
        s.to_owned()
    } else {
        format!("{s}:{DEFAULT_PORT}")
    }
}

/// A host label as it goes into session ids: letters, digits, `_` and `-`,
/// up to the first dot, at most 32 characters; `remote` when nothing is left.
pub fn host_label(raw: &str) -> String {
    let s: String = raw
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(32)
        .collect();
    if s.is_empty() {
        "remote".into()
    } else {
        s
    }
}

/// Compare two secrets without stopping at the first difference.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for (i, x) in a.iter().enumerate() {
        diff |= usize::from(x ^ b.get(i).copied().unwrap_or(!x));
    }
    diff == 0
}

/// How far a request has been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Read more.
    More,
    /// The whole request is in.
    Done,
    /// It is larger than the hub takes (413).
    TooLarge,
}

fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// Whether `buf` holds a whole request yet.
pub fn progress(buf: &[u8]) -> Progress {
    let Some(end) = head_end(buf) else {
        return if buf.len() > MAX_HEAD { Progress::TooLarge } else { Progress::More };
    };
    let len = content_length(&String::from_utf8_lossy(&buf[..end])).unwrap_or(0);
    if len > MAX_BODY {
        Progress::TooLarge
    } else if buf.len() >= end + len {
        Progress::Done
    } else {
        Progress::More
    }
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `GET /v1/ping`: answered with the version, no token needed.
    Ping,
    /// `POST /v1/hook` with a valid token and record.
    Hook(Box<HookRecord>),
}

/// Parse a whole request ([`Progress::Done`]). `Err` is the status to answer.
pub fn parse_request(buf: &[u8], token: &str) -> Result<Route, u16> {
    let end = head_end(buf).ok_or(400u16)?;
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    match (method, path) {
        ("GET", "/v1/ping") => return Ok(Route::Ping),
        ("POST", "/v1/hook") => {}
        _ => return Err(404),
    }
    let bearer = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("authorization"))
        .and_then(|(_, v)| v.trim().strip_prefix("Bearer "))
        .map(str::trim);
    if token.len() < MIN_TOKEN_LEN || !bearer.is_some_and(|b| ct_eq(b.as_bytes(), token.as_bytes())) {
        return Err(401);
    }
    let len = content_length(head).unwrap_or(0);
    if len > MAX_BODY {
        return Err(413);
    }
    let body = buf.get(end..end + len).ok_or(400u16)?;
    let rec: HookRecord = serde_json::from_slice(body).map_err(|_| 400u16)?;
    if rec.v != WIRE_VERSION {
        return Err(400);
    }
    Ok(Route::Hook(Box::new(rec)))
}

/// The response for a status code (`Connection: close`, empty body), or for
/// a ping, its body.
pub fn response(code: u16, body: &str) -> Vec<u8> {
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        _ => "Error",
    };
    format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// `POST /v1/hook` carrying `rec`.
pub fn hook_request(host_port: &str, token: &str, rec: &HookRecord) -> Vec<u8> {
    let body = serde_json::to_string(rec).unwrap_or_default();
    format!(
        "POST /v1/hook HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {token}\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// `GET /v1/ping`.
pub fn ping_request(host_port: &str) -> Vec<u8> {
    format!("GET /v1/ping HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n").into_bytes()
}

/// The status code of a response.
pub fn status_of(buf: &[u8]) -> Option<u16> {
    let line = buf.split(|b| *b == b'\n').next()?;
    std::str::from_utf8(line).ok()?.split(' ').nth(1)?.trim().parse().ok()
}

fn sanitize(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')).collect()
}

/// What the hub writes for one record, in the formats its own hook writes
/// (`clawdmeter hook`): the `events.log` line, the `attention` file's text,
/// and the `clear-<sid>` file name. They are written in this order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalWrites {
    /// `<now> <event> <host>.<sid> [<local dir>]\n`.
    pub event_line: Option<String>,
    /// `{"k","m","s"}\n`.
    pub attention: Option<String>,
    /// `clear-<host>.<sid>`.
    pub clear: Option<String>,
}

/// The session id the hub uses for a record's session.
pub fn local_session(rec: &HookRecord) -> String {
    if rec.sid == crate::attention::TEST_SESSION || rec.sid.is_empty() {
        rec.sid.clone()
    } else {
        format!("{}.{}", host_label(&rec.host), rec.sid)
    }
}

/// The hub's writes for `rec` at `now`, with `dir` the local account it
/// maps to (none: the line names no account, as an unattributed local one).
pub fn local_writes(rec: &HookRecord, now: u64, dir: Option<&Path>) -> LocalWrites {
    let sid = local_session(rec);
    let dir = dir.and_then(Path::to_str).filter(|d| !d.is_empty() && !d.chars().any(char::is_control));
    let event_line = rec.event.as_deref().map(|e| {
        let (name, s) = (sanitize(e), sanitize(&sid));
        let name = if name.is_empty() { "unknown".to_owned() } else { name };
        let s = if s.is_empty() { "-".to_owned() } else { s };
        match dir {
            Some(d) => format!("{now} {name} {s} {d}\n"),
            None => format!("{now} {name} {s}\n"),
        }
    });
    let attention = rec.alert.as_ref().map(|a| {
        let mut out = Map::new();
        out.insert("k".into(), Value::String(a.k.clone()));
        out.insert("m".into(), Value::String(a.m.clone()));
        out.insert("s".into(), if sid.is_empty() { Value::Null } else { Value::String(sid.clone()) });
        Value::Object(out).to_string().replace('\u{7f}', "\\u007f") + "\n"
    });
    let unsafe_id = sid.is_empty() || sid == "." || sid == ".." || sid.contains(['/', '\\', '\0', '\n']);
    let clear = (rec.clear && !unsafe_id).then(|| format!("clear-{sid}"));
    LocalWrites { event_line, attention, clear }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123";

    fn rec() -> HookRecord {
        HookRecord {
            v: WIRE_VERSION,
            host: "dev".into(),
            event: Some("UserPromptSubmit".into()),
            sid: "s1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn parses_requests() {
        let ok = hook_request("hub:1", TOKEN, &rec());
        let with = |from: &str, to: &str| String::from_utf8(ok.clone()).unwrap().replacen(from, to, 1).into_bytes();
        let big = {
            let mut r = rec();
            r.alert = Some(RemoteAlert { k: "permission".into(), m: "x".repeat(MAX_BODY) });
            hook_request("hub:1", TOKEN, &r)
        };
        type Case = (&'static str, Vec<u8>, Progress, Result<Route, u16>);
        let cases: Vec<Case> = vec![
            ("a record", ok.clone(), Progress::Done, Ok(Route::Hook(Box::new(rec())))),
            ("ping", ping_request("hub:1"), Progress::Done, Ok(Route::Ping)),
            ("half a body", ok[..ok.len() - 3].to_vec(), Progress::More, Err(400)),
            ("no head end", b"POST /v1/hook HTTP/1.1\r\n".to_vec(), Progress::More, Err(400)),
            ("wrong token", with(TOKEN, "0123456789abcdef0124"), Progress::Done, Err(401)),
            ("no token", with("Authorization", "X-Nothing"), Progress::Done, Err(401)),
            ("short token", with(TOKEN, "short"), Progress::Done, Err(401)),
            ("wrong path", with("/v1/hook", "/v1/other"), Progress::Done, Err(404)),
            ("bad json", with("\"v\":1", "\"v\":\"x\""), Progress::Done, Err(400)),
            ("other version", with("\"v\":1", "\"v\":2"), Progress::Done, Err(400)),
            ("too large", big, Progress::TooLarge, Err(413)),
        ];
        for (what, buf, progressed, parsed) in cases {
            assert_eq!(progress(&buf), progressed, "{what}");
            assert_eq!(parse_request(&buf, TOKEN), parsed, "{what}");
        }
        // A hub whose own token is too short takes nothing.
        assert_eq!(parse_request(&hook_request("h", "short", &rec()), "short"), Err(401));
        assert_eq!(status_of(&response(204, "")), Some(204));
    }

    #[test]
    fn hub_writes_what_its_own_hook_would() {
        let dir = Path::new("/Users/me/.claude-b");
        let alert = Some(RemoteAlert { k: "question".into(), m: "Pick\u{7f}".into() });
        // (record, local dir, event line, attention, clear)
        let cases = [
            (rec(), Some(dir), Some("9 UserPromptSubmit dev.s1 /Users/me/.claude-b\n"), None, None),
            (rec(), None, Some("9 UserPromptSubmit dev.s1\n"), None, None),
            (
                HookRecord { host: "acme.example.org".into(), alert: alert.clone(), clear: true, ..rec() },
                None,
                Some("9 UserPromptSubmit acme.s1\n"),
                Some("{\"k\":\"question\",\"m\":\"Pick\\u007f\",\"s\":\"acme.s1\"}\n"),
                Some("clear-acme.s1"),
            ),
            (
                HookRecord { sid: crate::attention::TEST_SESSION.into(), event: None, alert, ..rec() },
                None,
                None,
                Some("{\"k\":\"question\",\"m\":\"Pick\\u007f\",\"s\":\"clawdmeter-test\"}\n"),
                None,
            ),
            (
                HookRecord { sid: "../x".into(), event: Some("St op!".into()), clear: true, ..rec() },
                None,
                Some("9 Stop dev...x\n"),
                None,
                None,
            ),
            (
                HookRecord { host: "".into(), sid: "".into(), clear: true, ..rec() },
                None,
                Some("9 UserPromptSubmit -\n"),
                None,
                None,
            ),
        ];
        for (r, d, line, att, clear) in cases {
            let w = local_writes(&r, 9, d);
            assert_eq!(w.event_line.as_deref(), line, "{r:?}");
            assert_eq!(w.attention.as_deref(), att, "{r:?}");
            assert_eq!(w.clear.as_deref(), clear, "{r:?}");
        }
    }

    #[test]
    fn reads_hub_settings() {
        let home = Path::new("/home/me");
        let cfg = ConfigFile::parse(
            "hub_url = http://192.0.2.10/\nhub_token = file-token-0123456789\nhub_accounts = ~/.claude-b=b@x.test, \
             ~/.claude-c = u-c, broken\nhub_local = on\n",
        );
        let env = |k: &str| (k == "CLAWDMETER_HOST").then(|| "dev".to_owned());
        let h = HubConfig::from_config(&cfg, env, home);
        assert_eq!(h.sender(), Some(("192.0.2.10:47811", "file-token-0123456789")));
        assert_eq!(h.host.as_deref(), Some("dev"));
        assert!(h.local && h.listen.is_none());
        assert_eq!(h.account_hint(&home.join(".claude-c")), Some(&Identity::from_hint("u-c")));
        assert_eq!(h.accounts.len(), 2);
        let env = |k: &str| (k == "CLAWDMETER_HUB_TOKEN").then(|| "short".to_owned());
        assert_eq!(HubConfig::from_config(&cfg, env, home).sender(), None);
        assert_eq!(normalize_url("127.0.0.1:9"), "127.0.0.1:9");
        assert_eq!(host_label("host-1.lan"), "host-1");
        assert_eq!(host_label("..."), "remote");
    }
}
