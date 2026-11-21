//! The hub: hook events from other computers, over the network.
//!
//! One computer (the hub) has the device. Its daemon listens on `hub_listen`
//! ([`Hub`], a `std::net` thread: the workspace has no server crate and its
//! tokio has no `net`). Every other computer's `clawdmeter hook` sends its
//! record there ([`send_hook`]) instead of writing local files. The hub
//! writes each record into its own `events.log`, `attention` and `clear-*`
//! files ([`clawdmeter_core::remote::local_writes`]), stamped with its own
//! clock and the local account the record's sign-in matches
//! ([`IdentityMap`]), so the engine, the event tail and the attention inbox
//! work as they do for local hooks.
//!
//! The sender waits at most 250 ms to connect and 400 ms for the answer; a
//! failure (the hub down, a refused token) keeps it quiet for 30 s
//! (`<state>/hub-down`). Nothing is queued: events and alerts are stale by
//! the time a hub comes back.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clawdmeter_core::attention::TEST_SESSION;
use clawdmeter_core::identity::{self, IdentityMap};
use clawdmeter_core::remote::{self, HookRecord, HubConfig, Progress, RemoteAlert, Route, WIRE_VERSION};
use clawdmeter_core::{ConfigFile, Paths};
use serde_json::Value;

use crate::hooks;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const REPLY_TIMEOUT: Duration = Duration::from_millis(400);
/// How long the hook stays quiet after the hub didn't take a record (s).
pub const QUIET_SECS: u64 = 30;
/// How often the daemon retries a listen address it couldn't bind (s).
const RETRY_BIND_SECS: i64 = 30;
/// How long the hub gives one request, from accept to the last byte.
const REQUEST_DEADLINE: Duration = Duration::from_secs(1);
/// Connections handled at once; more are closed unanswered.
const MAX_CONNECTIONS: usize = 16;
/// How often the hub re-reads its accounts' sign-ins (s), so a re-login
/// never leaves a known id mapped to the wrong folder for long.
const REMAP_EVERY_SECS: u64 = 60;

/// The hub settings from the config file and the environment.
pub fn config(paths: &Paths) -> HubConfig {
    let cfg = ConfigFile::load(&paths.config_file).unwrap_or_else(|_| ConfigFile::parse(""));
    HubConfig::from_config(&cfg, |k| std::env::var(k).ok(), &paths.home)
}

/// This computer's label: `hub_host`, else its host name.
pub fn this_host(cfg: &HubConfig) -> String {
    let name = cfg
        .host
        .clone()
        .or_else(|| fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(mac_host_name)
        .unwrap_or_default();
    remote::host_label(name.trim())
}

/// macOS exports no `HOSTNAME`: ask for the local host name (only when
/// `hub_host` isn't set).
#[cfg(target_os = "macos")]
fn mac_host_name() -> Option<String> {
    let out = std::process::Command::new("scutil").args(["--get", "LocalHostName"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(not(target_os = "macos"))]
fn mac_host_name() -> Option<String> {
    None
}

/// Resolve `addr` within [`CONNECT_TIMEOUT`]: a name lookup can hang for
/// seconds (mDNS, an unreachable DNS server), and the hook must not.
fn resolve(addr: &str) -> io::Result<Vec<SocketAddr>> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Ok(vec![sa]);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let name = addr.to_owned();
    std::thread::spawn(move || {
        let _ = tx.send(name.to_socket_addrs().map(Iterator::collect::<Vec<_>>));
    });
    rx.recv_timeout(CONNECT_TIMEOUT).map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "name lookup timed out"))?
}

/// Connect to the first of `addr`'s addresses that answers (an IPv6 one can
/// be listed first while the hub listens on IPv4 only).
fn connect(addr: &str) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr}: no address"));
    for sa in resolve(addr)? {
        match TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Send one request; the status code and the body of the answer.
fn exchange(addr: &str, req: &[u8]) -> io::Result<(u16, String)> {
    let mut s = connect(addr)?;
    s.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    s.set_read_timeout(Some(REPLY_TIMEOUT))?;
    s.write_all(req)?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < remote::MAX_HEAD {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if buf.is_empty() => return Err(e),
            Err(_) => break,
        }
    }
    let code = remote::status_of(&buf).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no HTTP answer"))?;
    let text = String::from_utf8_lossy(&buf);
    Ok((code, text.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default()))
}

/// Send `rec` to the hub at `addr`; the status it answered (204 = taken).
pub fn send(addr: &str, token: &str, rec: &HookRecord) -> io::Result<u16> {
    exchange(addr, &remote::hook_request(addr, token, rec)).map(|(code, _)| code)
}

/// The config dir a hook run belongs to: `$CLAUDE_CONFIG_DIR` (hooks inherit
/// it), else the transcript's, else `~/.claude`.
fn hook_dir(v: &Value, home: &Path) -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty())
        .map(|d| clawdmeter_core::paths::expand_tilde(home, &d))
        .or_else(|| hooks::account_of(v).map(PathBuf::from))
        .unwrap_or_else(|| home.join(".claude"))
}

/// The hook's side: send this run's record to the hub, unless the hub
/// failed less than [`QUIET_SECS`] ago. Never prints, never fails.
pub fn send_hook(mode: hooks::Mode, input: &str, cfg: &HubConfig, paths: &Paths, now: u64) {
    let Some((addr, token)) = cfg.sender() else { return };
    let down = paths.state_dir.join("hub-down");
    let quiet = fs::read_to_string(&down).ok().and_then(|t| t.trim().parse::<u64>().ok());
    if quiet.is_some_and(|until| now < until) {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(input) else { return };
    let dir = hook_dir(&v, &paths.home);
    let acct =
        identity::read(&dir, &paths.home).or_else(|| cfg.account_hint(&dir).cloned()).map(identity::Identity::for_wire);
    let Some(rec) = hooks::record(mode, &v, &this_host(cfg), now, acct) else { return };
    if !matches!(send(addr, token, &rec), Ok(204)) {
        let _ = fs::create_dir_all(&paths.state_dir);
        let _ = fs::write(&down, (now + QUIET_SECS).to_string());
    }
}

/// `clawdmeter hub ping|test`: check the way to the hub from this computer.
pub fn cli(paths: &Paths, args: &[String]) -> i32 {
    let cfg = config(paths);
    let Some(addr) = cfg.url.as_deref() else {
        eprintln!("hub_url isn't set (in {} or CLAWDMETER_HUB)", paths.config_file.display());
        return 2;
    };
    match args.first().map(String::as_str) {
        Some("ping") => match exchange(addr, &remote::ping_request(addr)) {
            Ok((200, body)) => {
                println!("{addr}: {}", body.trim());
                0
            }
            Ok((code, _)) => {
                eprintln!("{addr}: answered {code}");
                1
            }
            Err(e) => {
                eprintln!("{addr}: {e}");
                1
            }
        },
        Some("test") => {
            let Some(token) = cfg.valid_token() else {
                eprintln!("hub_token isn't set or is shorter than {} characters", remote::MIN_TOKEN_LEN);
                return 2;
            };
            let host = this_host(&cfg);
            let rec = HookRecord {
                v: WIRE_VERSION,
                host: host.clone(),
                ts: clawdmeter_core::unix_now().max(0) as u64,
                sid: TEST_SESSION.to_owned(),
                alert: Some(RemoteAlert { k: "permission".into(), m: format!("Test alert from {host}") }),
                ..Default::default()
            };
            match send(addr, token, &rec) {
                Ok(204) => {
                    println!("{addr}: test alert sent");
                    0
                }
                Ok(401) => {
                    eprintln!("{addr}: the hub refused the token");
                    1
                }
                Ok(code) => {
                    eprintln!("{addr}: answered {code}");
                    1
                }
                Err(e) => {
                    eprintln!("{addr}: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("usage: clawdmeter hub ping|test");
            2
        }
    }
}

/// The hub's listener, as the status shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HubState {
    /// No `hub_listen`.
    #[default]
    Off,
    /// Taking records on this address.
    Listening(String),
    /// Can't listen (a bad token, a busy port); retried every 30 s.
    Failed(String),
}

/// What every connection reads and writes: the settings from the last
/// reload, and the account map built from them.
struct Shared {
    token: String,
    dirs: Vec<PathBuf>,
    state_dir: PathBuf,
    home: PathBuf,
    map: IdentityMap,
    mapped: Option<(Vec<PathBuf>, Instant)>,
    second: (Instant, u32),
}

/// The listening socket and its accept thread; dropping it stops both.
/// Connections run on threads of their own, each over within
/// [`REQUEST_DEADLINE`], so a slow client never holds up the others, and
/// dropping the listener never waits on one.
pub struct Listener {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<Shared>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Listener {
    /// Listen on `listen` (`host:port`; port 0 picks one).
    pub fn start(listen: &str, token: &str, dirs: Vec<PathBuf>, paths: &Paths) -> io::Result<Self> {
        let l = TcpListener::bind(listen)?;
        l.set_nonblocking(true)?;
        let addr = l.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(Shared {
            token: token.to_owned(),
            dirs,
            state_dir: paths.state_dir.clone(),
            home: paths.home.clone(),
            map: IdentityMap::default(),
            mapped: None,
            second: (Instant::now(), 0),
        }));
        let (s, sh) = (stop.clone(), shared.clone());
        let thread = std::thread::Builder::new().name("clawdmeter-hub".into()).spawn(move || serve(&l, &s, &sh))?;
        Ok(Listener { addr, stop, shared, thread: Some(thread) })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// New token or accounts, from a settings reload.
    pub fn update(&self, token: &str, dirs: &[PathBuf]) {
        if let Ok(mut g) = self.shared.lock() {
            if g.token != token {
                g.token = token.to_owned();
            }
            if g.dirs != dirs {
                g.dirs = dirs.to_vec();
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The accept loop checks the flag every 10 ms.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(l: &TcpListener, stop: &AtomicBool, shared: &Arc<Mutex<Shared>>) {
    let open = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match l.accept() {
            Ok((s, _)) => {
                if open.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    continue;
                }
                open.fetch_add(1, Ordering::Relaxed);
                let (sh, done) = (shared.clone(), open.clone());
                let spawned = std::thread::Builder::new().name("clawdmeter-hub-conn".into()).spawn(move || {
                    handle(s, &sh);
                    done.fetch_sub(1, Ordering::Relaxed);
                });
                if spawned.is_err() {
                    open.fetch_sub(1, Ordering::Relaxed);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                log::warn!("hub: accept failed: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Read one request within [`REQUEST_DEADLINE`]; the status to answer
/// early (400 when it never completes, 413 when it is too large).
fn read_request(s: &mut TcpStream) -> Result<Vec<u8>, u16> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match remote::progress(&buf) {
            Progress::Done => return Ok(buf),
            Progress::TooLarge => return Err(413),
            Progress::More => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            return Err(400);
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(400),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

fn handle(mut s: TcpStream, shared: &Mutex<Shared>) {
    let _ = s.set_nonblocking(false);
    let _ = s.set_write_timeout(Some(REQUEST_DEADLINE));
    let read = read_request(&mut s);
    let answer = {
        let Ok(mut sh) = shared.lock() else { return };
        let parsed = read.and_then(|buf| remote::parse_request(&buf, &sh.token));
        match parsed {
            Ok(Route::Ping) => remote::response(200, &format!("clawdmeter {}\n", crate::version())),
            Ok(Route::Hook(rec)) => {
                let code = if sh.over_limit() {
                    429
                } else {
                    match sh.ingest(&rec) {
                        Ok(()) => 204,
                        Err(e) => {
                            log::warn!("hub: couldn't write a record from {}: {e}", rec.host);
                            500
                        }
                    }
                };
                remote::response(code, "")
            }
            Err(code) => {
                log::debug!("hub: refused a request ({code})");
                remote::response(code, "")
            }
        }
    };
    let _ = s.write_all(&answer);
}

impl Shared {
    /// More than [`remote::MAX_PER_SEC`] records this second.
    fn over_limit(&mut self) -> bool {
        if self.second.0.elapsed() >= Duration::from_secs(1) {
            self.second = (Instant::now(), 0);
        }
        self.second.1 += 1;
        self.second.1 > remote::MAX_PER_SEC
    }

    /// Write `rec` into the hub's own hook files.
    fn ingest(&mut self, rec: &HookRecord) -> io::Result<()> {
        let stale = match &self.mapped {
            None => true,
            Some((d, at)) => *d != self.dirs || at.elapsed() >= Duration::from_secs(REMAP_EVERY_SECS),
        };
        if stale {
            self.map = IdentityMap::build(&self.dirs, &self.home);
            self.mapped = Some((self.dirs.clone(), Instant::now()));
        }
        let dir = rec.acct.as_ref().and_then(|a| self.map.lookup(a));
        let now = clawdmeter_core::unix_now().max(0) as u64;
        let w = remote::local_writes(rec, now, dir);
        log::debug!("hub: {} {:?} from {} → {:?}", rec.sid, rec.event, rec.host, dir);
        let state_dir = self.state_dir.as_path();
        fs::create_dir_all(state_dir)?;
        if let Some(line) = &w.event_line {
            hooks::append_event_line(state_dir, line)?;
        }
        if let Some(body) = &w.attention {
            hooks::write_attention(state_dir, body)?;
        }
        if let Some(name) = &w.clear {
            hooks::touch_clear_file(state_dir, name)?;
        }
        Ok(())
    }
}

/// The daemon's hub: starts, rebinds and stops the [`Listener`] as
/// `hub_listen` and `hub_token` change.
#[derive(Default)]
pub struct Hub {
    listener: Option<Listener>,
    listen: Option<String>,
    last_try: Option<i64>,
    /// For the status.
    pub state: HubState,
}

impl Hub {
    /// Follow the settings: called at start-up and on every settings reload.
    pub fn sync(&mut self, cfg: &HubConfig, dirs: &[PathBuf], paths: &Paths, now: i64) {
        let Some(listen) = cfg.listen.clone() else {
            self.listener = None;
            self.listen = None;
            self.set(HubState::Off);
            return;
        };
        let Some(token) = cfg.valid_token() else {
            self.listener = None;
            self.listen = None;
            self.set(HubState::Failed(format!(
                "hub_token must be set and at least {} characters",
                remote::MIN_TOKEN_LEN
            )));
            return;
        };
        if self.listen.as_ref() != Some(&listen) {
            self.listener = None;
            self.last_try = None;
            self.listen = Some(listen.clone());
        }
        if let Some(l) = &self.listener {
            l.update(token, dirs);
            return;
        }
        if self.last_try.is_some_and(|t| now - t < RETRY_BIND_SECS) {
            return;
        }
        self.last_try = Some(now);
        match Listener::start(&listen, token, dirs.to_vec(), paths) {
            Ok(l) => {
                self.set(HubState::Listening(l.addr().to_string()));
                self.listener = Some(l);
            }
            Err(e) => self.set(HubState::Failed(format!("can't listen on {listen}: {e}"))),
        }
    }

    fn set(&mut self, state: HubState) {
        if state != self.state {
            match &state {
                HubState::Off => log::info!("hub: off"),
                HubState::Listening(a) => log::info!("hub: listening on {a}"),
                HubState::Failed(e) => log::warn!("hub: {e}; retrying every {RETRY_BIND_SECS}s"),
            }
            self.state = state;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_from_another_computer_lands_in_the_hub_files() {
        let root = std::env::temp_dir().join(format!("cm-hub-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = Paths::under(&root);
        let acct_b = root.join(".claude-b");
        fs::create_dir_all(&acct_b).unwrap();
        fs::write(acct_b.join(".claude.json"), r#"{"oauthAccount":{"accountUuid":"u-b"}}"#).unwrap();
        let token = "0123456789abcdef0123";
        let hub = Listener::start("127.0.0.1:0", token, vec![root.join(".claude"), acct_b.clone()], &paths).unwrap();
        let addr = hub.addr().to_string();
        assert_eq!(exchange(&addr, &remote::ping_request(&addr)).unwrap().0, 200);
        let mode = hooks::Mode::parse(&["alert".into(), "event".into()]);
        let input = serde_json::json!({"hook_event_name": "Notification", "session_id": "s1", "message": "Allow?"});
        let id = identity::Identity { uuid: Some("u-b".into()), ..Default::default() };
        let rec = hooks::record(mode, &input, "dev", 1, Some(id)).unwrap();
        assert_eq!(send(&addr, "wrong-token-0123456789", &rec).unwrap(), 401);
        assert_eq!(send(&addr, token, &rec).unwrap(), 204);
        let log = fs::read_to_string(paths.events_log()).unwrap();
        let ev = clawdmeter_core::events::parse_line(log.trim_end()).unwrap();
        assert_eq!((ev.session.as_str(), ev.account.as_deref()), ("dev.s1", Some(acct_b.as_path())));
        assert_eq!(
            fs::read_to_string(paths.attention_file()).unwrap(),
            "{\"k\":\"permission\",\"m\":\"Allow?\",\"s\":\"dev.s1\"}\n"
        );
        drop(hub);
        let _ = fs::remove_dir_all(&root);
    }
}
