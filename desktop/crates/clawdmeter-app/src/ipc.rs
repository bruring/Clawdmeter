//! How the windows see the daemon.
//!
//! The tray process owns the daemon. The app window is its own GPUI process
//! (`clawdmeter --window [page]`), because GPUI ends its event loop when the
//! last window closes on Linux and Windows, and a tray app has no window most
//! of the time. There is only ever one window process (`window.lock`); the
//! tray switches it between pages rather than opening another. The two sides
//! talk through small files in the state dir, the same way the hook and the
//! daemon already do:
//!
//! - `app-status.json`: the tray process rewrites it whenever the status
//!   changes (and at least every few seconds, so the window can tell a live
//!   daemon from a dead one by `updated_at`).
//! - `app-commands`: the window appends one command per line; the tray
//!   process takes the file (rename, read, delete) every tick.
//! - `window-commands`: `show <page>` lines for the window process, taken the
//!   same way: bring the window to the front on that page.

use std::fs;
use std::io::{self, Write as _};
use std::path::PathBuf;

use clawdmeter_core::Paths;
use serde::{Deserialize, Serialize};

use crate::art::TrayState;
use crate::daemon::{LinkState, Status};

/// `DATA_FRESH_MS` in the firmware: no write for this long reads as stale.
pub const DATA_FRESH_SECS: i64 = 90;
/// A status file older than this means the tray process isn't running.
pub const STATUS_ALIVE_SECS: i64 = 12;

/// `<state>/app-status.json`.
pub fn status_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("app-status.json")
}

/// `<state>/app-commands`.
pub fn commands_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("app-commands")
}

/// `<cache>/device-firmware.json`: what the last flash or `info` probe saw.
pub fn firmware_record_file(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("device-firmware.json")
}

/// One account's last poll error. A status file from an older build (errors
/// as `[dir, message]` pairs) still reads, as not retrying.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountError {
    /// Config dir.
    pub dir: String,
    /// What to tell the user.
    pub message: String,
    /// The app retries on its own (shown as "Retrying"); otherwise the user
    /// has to act.
    #[serde(default)]
    pub retrying: bool,
}

/// One account in the rows that list them all (the Overview's Accounts card,
/// both trays), with two or more accounts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRow {
    /// The folder.
    pub dir: String,
    /// The sign-in's email domain when known (`identity::labels`), else empty:
    /// [`AccountRow::name`] falls back to the folder.
    #[serde(default)]
    pub label: String,
    /// The device shows this one.
    pub shown: bool,
    /// A Codex account.
    #[serde(default)]
    pub codex: bool,
    /// The 5-hour window's % (`None`: no limit, spend-capped, or no data).
    pub session_pct: Option<u32>,
    /// It is running.
    #[serde(default)]
    pub window_started: bool,
    /// When it resets.
    pub session_resets_at: Option<i64>,
    /// The week's %.
    pub week_pct: Option<u32>,
    /// When the week resets.
    pub week_resets_at: Option<i64>,
    /// A spend-capped account's spend %.
    pub spend_pct: Option<u32>,
    /// Reset credits held, when known.
    pub resets_held: Option<u32>,
    /// What each held one resets (`ResetKind::code` letters), when known.
    #[serde(default)]
    pub resets_kinds: String,
    /// The numbers are old (polls keep missing).
    pub stale: bool,
    /// No numbers, and the last check failed in a way the user has to fix
    /// (signed out, a refused or expired sign-in).
    #[serde(default)]
    pub signed_out: bool,
    /// The most room left of the Claude accounts with fresh usage windows
    /// (the higher of the session and week %, lowest), with two or more to
    /// compare. Spend caps and Codex accounts measure other things.
    #[serde(default)]
    pub most_room: bool,
}

impl AccountRow {
    /// What the row is called: the label, else the folder (`~` for home);
    /// a Codex account says so, as the "Show on device" menu does.
    pub fn name(&self, home: &std::path::Path) -> String {
        let name = if self.label.is_empty() { crate::tilde(&self.dir, home) } else { self.label.clone() };
        if self.codex {
            format!("{name} (Codex)")
        } else {
            name
        }
    }

    /// The numbers in one line, as every surface writes them:
    /// "Week 95% · resets Thu 00:00 · 0 resets", "5h 40% · Week 30% · resets
    /// Thu 00:00", "Spend 40%", "No data yet". Times are in `tz`.
    pub fn text<Tz: chrono::TimeZone>(&self, tz: &Tz) -> String
    where
        Tz::Offset: std::fmt::Display,
    {
        let when = |t: Option<i64>| {
            t.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| format!(" · resets {}", t.with_timezone(tz).format("%a %H:%M")))
                .unwrap_or_default()
        };
        let mut s = match (self.spend_pct, self.session_pct, self.week_pct) {
            (Some(p), _, _) => format!("Spend {p}%"),
            (None, Some(_), w) if !self.window_started => {
                format!("5h not started · Week {}{}", pct(w), when(self.week_resets_at))
            }
            (None, Some(p), w) => format!("5h {p}% · Week {}{}", pct(w), when(self.week_resets_at)),
            (None, None, Some(w)) => format!("Week {w}%{}", when(self.week_resets_at)),
            (None, None, None) if self.signed_out => return "Sign-in needed".into(),
            (None, None, None) => return "No data yet".into(),
        };
        if let Some(n) = self.resets_held {
            s.push_str(&format!(" · {}", resets_words(n, &self.resets_kinds)));
        }
        if self.stale {
            s.push_str(" · out of date");
        }
        s
    }

    /// How full its fullest window is (`None`: no numbers).
    fn fullest(&self) -> Option<u32> {
        self.spend_pct.or(match (self.session_pct.filter(|_| self.window_started), self.week_pct) {
            (Some(s), Some(w)) => Some(s.max(w)),
            (s, w) => s.or(w),
        })
    }
}

/// Reset credits held, in words, naming their kinds when every one's kind
/// is known: "1 weekly + 1 Fable resets", "2 weekly resets", "1 Fable
/// reset", else "3 resets". Short enough for the macOS panel's rows.
pub fn resets_words(held: u32, kinds: &str) -> String {
    let s = if held == 1 { "" } else { "s" };
    if held == 0 || kinds.chars().count() != held as usize {
        return format!("{held} reset{s}");
    }
    let count = |k: char| kinds.chars().filter(|c| *c == k).count();
    let parts: Vec<String> = [('w', "weekly"), ('f', "Fable"), ('o', "other")]
        .into_iter()
        .filter(|(k, _)| count(*k) > 0)
        .map(|(k, w)| format!("{} {w}", count(k)))
        .collect();
    format!("{} reset{s}", parts.join(" + "))
}

fn pct(p: Option<u32>) -> String {
    p.map_or_else(|| "--".into(), |p| format!("{p}%"))
}

/// The rows for every account, or none with fewer than two. `labels` are
/// the daemon's short names.
pub fn account_rows(e: &clawdmeter_core::EngineStatus, labels: &[(PathBuf, String)], now: i64) -> Vec<AccountRow> {
    use clawdmeter_core::usage::MeterKind;
    if e.accounts.len() < 2 {
        return Vec::new();
    }
    let mut rows: Vec<AccountRow> = e
        .accounts
        .iter()
        .map(|a| {
            let r = a.reading.as_ref();
            let spend = r.filter(|r| r.meter_kind() == MeterKind::Spend);
            // Reset credits where the main screens show them: a weekly-only plan.
            let credits = r
                .filter(|r| r.no_session_limit())
                .and_then(|r| r.resets.as_ref())
                .map(|c| chrono::DateTime::from_timestamp(now, 0).map_or_else(|| c.clone(), |at| c.at(at)));
            let resets = credits.as_ref().map(|c| c.held);
            let resets_kinds: String =
                credits.as_ref().map(|c| c.kinds.iter().map(|k| k.code()).collect()).unwrap_or_default();
            AccountRow {
                dir: a.dir.display().to_string(),
                label: labels.iter().find(|(d, _)| *d == a.dir).map(|(_, l)| l.clone()).unwrap_or_default(),
                shown: e.active_account.as_ref() == Some(&a.dir),
                codex: a.codex,
                session_pct: r.filter(|r| spend.is_none() && !r.no_session_limit()).map(|r| r.session_pct()),
                window_started: r.is_some_and(|r| r.window_started()),
                session_resets_at: r.and_then(|r| r.session.as_ref()).and_then(|w| w.resets_at).map(|t| t.timestamp()),
                week_pct: r.filter(|_| spend.is_none()).and_then(|r| r.week.as_ref()).map(|w| w.pct),
                week_resets_at: r
                    .filter(|_| spend.is_none())
                    .and_then(|r| r.week.as_ref())
                    .and_then(|w| w.resets_at)
                    .map(|t| t.timestamp()),
                spend_pct: spend.map(|r| r.session_pct()),
                resets_held: resets,
                resets_kinds,
                stale: a.stale,
                signed_out: r.is_none() && e.errors.iter().any(|x| x.dir == a.dir && !x.retrying),
                most_room: false,
            }
        })
        .collect();
    let fresh: Vec<(usize, u32)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.stale && !r.codex && r.spend_pct.is_none())
        .filter_map(|(i, r)| Some((i, r.fullest()?)))
        .collect();
    if fresh.len() >= 2 {
        if let Some(&(i, _)) = fresh.iter().min_by_key(|(_, f)| *f) {
            rows[i].most_room = true;
        }
    }
    rows
}

/// The window-facing view of [`Status`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    /// Unix seconds this snapshot was written.
    pub updated_at: i64,
    /// `connected` | `connecting` | `searching` | `no_adapter` | `starting`.
    pub link: String,
    /// Device address / id when connected.
    pub device_key: Option<String>,
    /// Device name when connected.
    pub device_name: Option<String>,
    /// Adapter problem text (`no_adapter`).
    pub link_detail: Option<String>,
    /// Account the device shows.
    pub account: Option<String>,
    /// Session % (`None` with no 5-hour limit).
    pub session_pct: Option<u32>,
    /// Weekly %.
    pub week_pct: Option<u32>,
    /// The session window has started.
    pub window_started: bool,
    /// The account has no 5-hour limit (verified Ultra): no session meter.
    #[serde(default)]
    pub no_session_limit: bool,
    /// Unix seconds the numbers were read (age = now - this).
    pub read_at: Option<i64>,
    /// Shown account missed three polls.
    pub stale: bool,
    /// Every account is logged out.
    pub logged_out: bool,
    /// Per-account errors.
    pub errors: Vec<AccountError>,
    /// Attention alert up.
    pub attention: bool,
    /// Hook activity 0-3.
    pub activity: u8,
    /// Sessions active in the last 5 min.
    pub sessions: u32,
    /// Hook events have been seen.
    pub hooks: bool,
    /// `claude` CLI in use.
    pub claude: Option<String>,
    /// Last successful device write.
    pub last_write_at: Option<i64>,
    /// Latest firmware release, when the update check found one.
    pub latest_firmware: Option<String>,
    /// Firmware the device last reported (flash or `info`).
    pub device_firmware: Option<String>,
    /// Tray state name (see [`TrayState`]).
    pub tray: String,
    /// The earliest a refresh can poll the shown account (the per-account
    /// gap and backoff); `None`: right away.
    #[serde(default)]
    pub refresh_at: Option<i64>,
    /// The usage endpoint blocked the shown account until then (a 429).
    #[serde(default)]
    pub paused_until: Option<i64>,
    /// The shown account is being polled.
    #[serde(default)]
    pub polling: bool,
    /// No tray icon: the window is the app, and closing it quits.
    #[serde(default)]
    pub windowed: bool,
    /// Unix seconds the session window resets (`None`: not started).
    #[serde(default)]
    pub session_resets_at: Option<i64>,
    /// Unix seconds the weekly window resets (`None`: unknown).
    #[serde(default)]
    pub week_resets_at: Option<i64>,
    /// Fable's weekly % of its own limit, drawn inside the weekly meter
    /// (`None`: unknown, no Fable limit, or a spend-capped account).
    #[serde(default)]
    pub fable_pct: Option<u32>,
    /// A spend-capped account: `session_pct` is the spend % (it can pass
    /// 100), and there is no session or weekly window to show.
    #[serde(default)]
    pub spend: bool,
    /// Unix seconds the spend period resets (`None`: unknown).
    #[serde(default)]
    pub spend_resets_at: Option<i64>,
    /// The shown account is a Codex account (a trial).
    #[serde(default)]
    pub codex: bool,
    /// Its reset credits held, when known.
    #[serde(default)]
    pub resets_held: Option<u32>,
    /// Unix seconds each held reset credit expires, soonest first (at most
    /// `ResetCredits::MAX_EXPIRIES`); empty when unknown.
    #[serde(default)]
    pub resets_expiries: Vec<i64>,
    /// What each of `resets_expiries` resets, one `ResetKind::code` letter
    /// each (`w` weekly, `f` Fable only); empty when all alike (Codex).
    #[serde(default)]
    pub resets_kinds: String,
    /// Every account, with two or more ([`account_rows`]).
    #[serde(default)]
    pub accounts: Vec<AccountRow>,
}

impl StatusSnapshot {
    /// Build from the daemon's status.
    pub fn from_status(s: &Status, now: i64) -> Self {
        let (link, device_key, device_name, link_detail) = match &s.link {
            LinkState::Starting => ("starting", None, None, None),
            LinkState::NoAdapter(e) => ("no_adapter", None, None, Some(e.clone())),
            LinkState::Searching => ("searching", None, None, None),
            LinkState::Connecting => ("connecting", None, None, None),
            LinkState::Connected { key, name } => ("connected", Some(key.clone()), name.clone(), None),
        };
        let e = &s.engine;
        let reading = e.reading.as_ref();
        // Credits that expired since the poll are gone (`ResetCredits::at`).
        let resets = reading
            .and_then(|r| r.resets.as_ref())
            .map(|c| chrono::DateTime::from_timestamp(now, 0).map_or_else(|| c.clone(), |at| c.at(at)));
        StatusSnapshot {
            updated_at: now,
            link: link.into(),
            device_key,
            device_name,
            link_detail,
            account: e.active_account.as_ref().map(|p| p.display().to_string()),
            session_pct: reading.filter(|r| !r.no_session_limit()).map(|r| r.session_pct()),
            week_pct: reading.and_then(|r| r.week.as_ref()).map(|w| w.pct),
            window_started: reading.is_some_and(|r| r.window_started()),
            no_session_limit: reading.is_some_and(|r| r.no_session_limit()),
            read_at: e.age_secs.map(|a| now - a),
            stale: e.stale,
            logged_out: e.logged_out,
            errors: e
                .errors
                .iter()
                .map(|x| AccountError {
                    dir: x.dir.display().to_string(),
                    message: x.message.clone(),
                    retrying: x.retrying,
                })
                .collect(),
            attention: e.attention,
            activity: e.activity,
            sessions: e.sessions,
            hooks: e.hooks,
            claude: s.claude.as_ref().map(|p| p.display().to_string()),
            last_write_at: s.last_write_at,
            latest_firmware: None,
            device_firmware: None,
            tray: format!("{:?}", tray_state(s, now)),
            refresh_at: e.refresh_at,
            paused_until: e.paused_until,
            polling: e.polling,
            windowed: false,
            session_resets_at: reading
                .and_then(|r| r.session.as_ref())
                .and_then(|w| w.resets_at)
                .map(|t| t.timestamp()),
            week_resets_at: reading.and_then(|r| r.week.as_ref()).and_then(|w| w.resets_at).map(|t| t.timestamp()),
            fable_pct: reading.filter(|r| !r.is_spend_only()).and_then(|r| r.fable()).map(|w| w.pct),
            spend: reading.is_some_and(|r| r.meter_kind() == clawdmeter_core::usage::MeterKind::Spend),
            spend_resets_at: reading
                .filter(|r| r.meter_kind() == clawdmeter_core::usage::MeterKind::Spend)
                .and_then(|r| r.spend.as_ref())
                .and_then(|sp| sp.resets_at)
                .map(|t| t.timestamp()),
            codex: reading.is_some_and(|r| r.provider == clawdmeter_core::usage::Provider::Codex),
            resets_held: resets.as_ref().map(|c| c.held),
            resets_kinds: resets.as_ref().map(|c| c.kinds.iter().map(|k| k.code()).collect()).unwrap_or_default(),
            resets_expiries: resets.map(|c| c.expiries.iter().map(|t| t.timestamp()).collect()).unwrap_or_default(),
            accounts: account_rows(e, &s.labels, now),
        }
    }

    /// The reset credits held, where every surface shows them (the device's
    /// Resets card, the Overview, both trays): a weekly-only plan whose count
    /// is known, Codex's credits or Claude's limit-reset grants. With a 5-hour
    /// window the week takes that place; a spend cap has none.
    pub fn shown_resets(&self) -> Option<u32> {
        self.resets_held.filter(|_| self.no_session_limit && !self.spend)
    }

    /// Fable's % for the strip inside the Weekly bar, where the Overview and
    /// the macOS tray panel draw it (as the device does): a Claude account's
    /// weekly meter, never a Codex account's (Fable is Claude's) or a
    /// spend-capped one's (no weekly bar).
    pub fn shown_fable(&self) -> Option<u32> {
        self.fable_pct.filter(|_| !self.codex && !self.spend)
    }

    /// Seconds since the numbers were read.
    pub fn age(&self, now: i64) -> Option<i64> {
        self.read_at.map(|t| (now - t).max(0))
    }

    /// Equal apart from `updated_at`.
    pub fn same_content(&self, other: &StatusSnapshot) -> bool {
        StatusSnapshot { updated_at: 0, ..self.clone() } == StatusSnapshot { updated_at: 0, ..other.clone() }
    }

    /// The tray process is alive (the file was refreshed recently).
    pub fn alive(&self, now: i64) -> bool {
        now - self.updated_at <= STATUS_ALIVE_SECS
    }

    /// Parsed tray state.
    pub fn tray_state(&self) -> TrayState {
        match self.tray.as_str() {
            "Connected" => TrayState::Connected,
            "Stale" => TrayState::Stale,
            "AuthError" => TrayState::AuthError,
            "Attention" => TrayState::Attention,
            _ => TrayState::Searching,
        }
    }

    /// An update is available for the device.
    pub fn update_available(&self) -> Option<&str> {
        let latest = self.latest_firmware.as_deref()?;
        match self.device_firmware.as_deref() {
            // Local builds report `dev` (CLAWD_VERSION's default): not a
            // release, so nothing to compare against.
            Some(have) if !have.trim_start_matches(['v', 'V']).starts_with(|c: char| c.is_ascii_digit()) => None,
            Some(have) if !clawdmeter_flash::manifest::is_newer(latest, have) => None,
            Some(_) => Some(latest),
            None => None,
        }
    }

    /// The tag of a release newer than this app at `app_version`. The update
    /// check stores one tag for the firmware and the app: they ship together.
    pub fn app_update(&self, app_version: &str) -> Option<String> {
        let latest = clawdmeter_flash::release::release_tag(self.latest_firmware.as_deref()?)?;
        clawdmeter_flash::manifest::is_newer(&latest, app_version).then_some(latest)
    }
}

/// Attention wins, then auth, then the link, then freshness.
pub fn tray_state(s: &Status, now: i64) -> TrayState {
    if s.engine.attention {
        return TrayState::Attention;
    }
    if s.engine.logged_out {
        return TrayState::AuthError;
    }
    if !matches!(s.link, LinkState::Connected { .. }) {
        return TrayState::Searching;
    }
    let fresh = s.last_write_at.is_some_and(|t| now - t <= DATA_FRESH_SECS);
    if s.engine.stale || s.engine.reading.is_none() || !fresh {
        TrayState::Stale
    } else {
        TrayState::Connected
    }
}

/// Write atomically (tmp + rename).
pub fn write_status(paths: &Paths, snap: &StatusSnapshot) -> io::Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    let path = status_file(paths);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(snap).map_err(io::Error::other)?)?;
    fs::rename(tmp, path)
}

/// Read the latest snapshot (None when missing or unreadable).
pub fn read_status(paths: &Paths) -> Option<StatusSnapshot> {
    serde_json::from_slice(&fs::read(status_file(paths)).ok()?).ok()
}

/// Requests a window sends to the tray process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    /// Poll `/usage` now.
    Refresh,
    /// Drop the BLE link and look again.
    Reconnect,
    /// Forget the cached device address, then look again.
    Forget,
    /// Open the app window on a page (see [`crate::Page::parse`]).
    Open(String),
    /// Re-run the firmware update check now.
    CheckUpdate,
    /// The config file changed: re-read it now, not at the next periodic
    /// re-read, so a device setting reaches the device at once.
    SettingsChanged,
    /// Stop the daemon and exit.
    Quit,
}

impl AppCommand {
    /// Wire form, one line.
    pub fn encode(&self) -> String {
        match self {
            AppCommand::Refresh => "refresh".into(),
            AppCommand::Reconnect => "reconnect".into(),
            AppCommand::Forget => "forget".into(),
            AppCommand::Open(w) => format!("open {w}"),
            AppCommand::CheckUpdate => "check-update".into(),
            AppCommand::SettingsChanged => "settings-changed".into(),
            AppCommand::Quit => "quit".into(),
        }
    }

    /// Parse one line.
    pub fn decode(line: &str) -> Option<Self> {
        let mut it = line.split_whitespace();
        Some(match (it.next()?, it.next()) {
            ("refresh", None) => AppCommand::Refresh,
            ("reconnect", None) => AppCommand::Reconnect,
            ("forget", None) => AppCommand::Forget,
            ("open", Some(w)) if crate::Page::parse(w).is_some() => AppCommand::Open(w.to_owned()),
            ("check-update", None) => AppCommand::CheckUpdate,
            ("settings-changed", None) => AppCommand::SettingsChanged,
            ("quit", None) => AppCommand::Quit,
            _ => return None,
        })
    }
}

/// Append a command for the tray process.
pub fn send_command(paths: &Paths, cmd: &AppCommand) -> io::Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(commands_file(paths))?;
    writeln!(f, "{}", cmd.encode())
}

/// Take every pending command. The file is renamed first, so a window that
/// opens it after the rename starts a fresh one; a window that opened it
/// just before can still lose its line to the removal. For
/// `settings-changed` that only means waiting for the daemon's periodic
/// re-read.
pub fn take_commands(paths: &Paths) -> Vec<AppCommand> {
    let path = commands_file(paths);
    let taking = path.with_extension("taking");
    if fs::rename(&path, &taking).is_err() {
        return Vec::new();
    }
    let text = fs::read_to_string(&taking).unwrap_or_default();
    let _ = fs::remove_file(&taking);
    text.lines().filter_map(AppCommand::decode).collect()
}

/// `<state>/app.lock`: held for the life of the process that runs the daemon.
pub fn lock_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("app.lock")
}

/// Take the single-instance lock. `Ok(None)` means another tray or
/// `--daemon` process already runs the daemon (two would fight over the BLE
/// link and the command file). Keep the returned file open; the OS releases
/// the lock when the process exits, however it exits.
pub fn acquire_instance_lock(paths: &Paths) -> io::Result<Option<fs::File>> {
    fs::create_dir_all(&paths.state_dir)?;
    let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_file(paths))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// `<state>/window-commands`.
pub fn window_commands_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("window-commands")
}

/// `<state>/window.lock`: held by the app window process.
pub fn window_lock_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("window.lock")
}

/// Ask the running window process to come to the front on `page`.
pub fn send_window_command(paths: &Paths, page: crate::Page) -> io::Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(window_commands_file(paths))?;
    writeln!(f, "show {}", page.as_str())
}

/// Take every pending `show <page>` (rename first, as [`take_commands`]).
pub fn take_window_commands(paths: &Paths) -> Vec<crate::Page> {
    let path = window_commands_file(paths);
    let taking = path.with_extension("taking");
    if fs::rename(&path, &taking).is_err() {
        return Vec::new();
    }
    let text = fs::read_to_string(&taking).unwrap_or_default();
    let _ = fs::remove_file(&taking);
    text.lines().filter_map(|l| l.strip_prefix("show ")).filter_map(|p| crate::Page::parse(p.trim())).collect()
}

/// Take the window lock. `Ok(None)`: a window process is already running
/// (send it a [`send_window_command`] instead). Keep the file open.
pub fn acquire_window_lock(paths: &Paths) -> io::Result<Option<fs::File>> {
    fs::create_dir_all(&paths.state_dir)?;
    let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(window_lock_file(paths))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Whether some process has the app window open (holds the window lock).
pub fn window_open_elsewhere(paths: &Paths) -> bool {
    matches!(acquire_window_lock(paths), Ok(None))
}

/// What the device last told us about its firmware.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareRecord {
    /// PlatformIO env.
    pub env: String,
    /// Version.
    pub ver: String,
}

/// Remember the device's env and version (after a flash or an `info` probe).
pub fn save_firmware_record(paths: &Paths, rec: &FirmwareRecord) -> io::Result<()> {
    fs::create_dir_all(&paths.cache_dir)?;
    fs::write(firmware_record_file(paths), serde_json::to_vec(rec).map_err(io::Error::other)?)
}

/// The remembered firmware, if any.
pub fn load_firmware_record(paths: &Paths) -> Option<FirmwareRecord> {
    serde_json::from_slice(&fs::read(firmware_record_file(paths)).ok()?).ok()
}

/// `<cache>/latest-release`: tag of the last release the update check saw.
pub fn latest_release_file(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("latest-release")
}

/// Read the cached latest release tag.
pub fn load_latest_release(paths: &Paths) -> Option<String> {
    fs::read_to_string(latest_release_file(paths)).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_open_elsewhere_follows_the_lock() {
        let dir = std::env::temp_dir().join(format!("clawdmeter-winlock-{}", std::process::id()));
        let paths = Paths::under(&dir);
        assert!(!window_open_elsewhere(&paths));
        let held = acquire_window_lock(&paths).unwrap().expect("free");
        assert!(window_open_elsewhere(&paths));
        drop(held);
        assert!(!window_open_elsewhere(&paths));
        let _ = fs::remove_dir_all(dir);
    }
    use clawdmeter_core::usage::{SessionLimit, UsageReading, Window};
    use clawdmeter_core::EngineStatus;

    #[test]
    fn every_account_gets_a_row() {
        use chrono::{DateTime, Utc};
        use clawdmeter_core::usage::{ResetCredits, Spend};
        use clawdmeter_core::AccountStatus;
        let now = 1_795_000_000;
        let at = |t: i64| DateTime::<Utc>::from_timestamp(t, 0);
        // Thursday 2026-11-26 00:00 UTC.
        let thu = 1_795_651_200;
        let week = |pct| Some(Window { pct, resets_at: at(thu) });
        let acct = |dir: &str, reading: Option<UsageReading>, stale| AccountStatus {
            dir: dir.into(),
            codex: false,
            reading,
            age_secs: Some(60),
            stale,
        };
        let ultra = UsageReading {
            week: week(95),
            session_limit: SessionLimit::Absent,
            resets: Some(ResetCredits { held: 0, ..Default::default() }),
            ..Default::default()
        };
        let max = UsageReading {
            session: Some(Window { pct: 40, resets_at: at(now + 3600) }),
            week: week(30),
            ..Default::default()
        };
        let spend =
            UsageReading { spend: Some(Spend { pct: 12, resets_at: None, period: None }), ..Default::default() };
        let e = EngineStatus {
            active_account: Some("/h/.claude".into()),
            accounts: vec![
                acct("/h/.claude", Some(ultra), false),
                acct("/h/.claude-b", Some(max.clone()), false),
                acct("/h/.claude-c", Some(spend), false),
                acct("/h/.claude-d", Some(max), true),
                acct("/h/.claude-e", None, true),
                acct("/h/.claude-f", None, false),
            ],
            errors: vec![clawdmeter_core::AccountError {
                dir: "/h/.claude-e".into(),
                message: "signed out".into(),
                retrying: false,
            }],
            ..Default::default()
        };
        let labels = vec![(PathBuf::from("/h/.claude"), "acme.ai".to_owned())];
        let rows = account_rows(&e, &labels, now);
        let home = std::path::Path::new("/h");
        let got: Vec<(String, String, bool, bool)> =
            rows.iter().map(|r| (r.name(home), r.text(&Utc), r.shown, r.most_room)).collect();
        let want = [
            ("acme.ai", "Week 95% · resets Thu 00:00 · 0 resets", true, false),
            ("~/.claude-b", "5h 40% · Week 30% · resets Thu 00:00", false, true),
            ("~/.claude-c", "Spend 12%", false, false),
            ("~/.claude-d", "5h 40% · Week 30% · resets Thu 00:00 · out of date", false, false),
            ("~/.claude-e", "Sign-in needed", false, false),
            ("~/.claude-f", "No data yet", false, false),
        ];
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert_eq!((g.0.as_str(), g.1.as_str(), g.2, g.3), w);
        }
        // One account: no rows (the meters already are it).
        let one = EngineStatus { accounts: e.accounts[..1].to_vec(), ..e.clone() };
        assert!(account_rows(&one, &labels, now).is_empty());
    }

    fn status(link: LinkState, reading: bool, last_write: Option<i64>) -> Status {
        Status {
            link,
            engine: EngineStatus {
                reading: reading.then(|| UsageReading {
                    session: Some(Window { pct: 3, resets_at: None }),
                    week: Some(Window { pct: 12, resets_at: None }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            last_write_at: last_write,
            claude: None,
            hub: Default::default(),
            labels: Vec::new(),
        }
    }

    fn connected() -> LinkState {
        LinkState::Connected { key: "AA:BB".into(), name: Some("Clawdmeter".into()) }
    }

    #[test]
    fn reset_credits_show_on_a_weekly_only_codex_plan_until_they_expire() {
        use clawdmeter_core::usage::{Provider, ResetCredits, SessionLimit};
        let day =
            |d: u32| chrono::NaiveDate::from_ymd_opt(2026, 12, d).unwrap().and_hms_opt(12, 0, 0).unwrap().and_utc();
        let mut s = status(connected(), true, None);
        s.engine.reading = Some(UsageReading {
            week: Some(Window { pct: 20, resets_at: None }),
            session_limit: SessionLimit::Absent,
            provider: Provider::Codex,
            resets: Some(ResetCredits { held: 3, expiries: vec![day(1), day(5), day(12)], kinds: vec![] }),
            ..Default::default()
        });
        let snap = StatusSnapshot::from_status(&s, day(1).timestamp() - 60);
        assert_eq!((snap.shown_resets(), snap.resets_expiries.len()), (Some(3), 3));
        // Kept past the soonest expiry (no poll since): that credit is gone.
        let later = StatusSnapshot::from_status(&s, day(2).timestamp());
        assert_eq!(
            (later.shown_resets(), later.resets_expiries),
            (Some(2), vec![day(5).timestamp(), day(12).timestamp()])
        );
        // Only a weekly-only plan with a known count shows them: Codex's
        // credits, or Claude's limit-reset grants alike.
        assert_eq!(StatusSnapshot { no_session_limit: false, ..snap.clone() }.shown_resets(), None);
        assert_eq!(StatusSnapshot { codex: false, ..snap.clone() }.shown_resets(), Some(3));
        assert_eq!(StatusSnapshot { spend: true, ..snap.clone() }.shown_resets(), None);
        assert_eq!(StatusSnapshot { resets_held: None, ..snap }.shown_resets(), None);
    }

    #[test]
    fn tray_state_priorities() {
        let now = 1_000;
        assert_eq!(tray_state(&status(connected(), true, Some(now - 10)), now), TrayState::Connected);
        assert_eq!(tray_state(&status(connected(), true, Some(now - 91)), now), TrayState::Stale);
        assert_eq!(tray_state(&status(connected(), false, Some(now)), now), TrayState::Stale);
        assert_eq!(tray_state(&status(LinkState::Searching, true, Some(now)), now), TrayState::Searching);
        let mut s = status(connected(), true, Some(now));
        s.engine.logged_out = true;
        assert_eq!(tray_state(&s, now), TrayState::AuthError);
        s.engine.attention = true;
        assert_eq!(tray_state(&s, now), TrayState::Attention);
    }

    #[test]
    fn account_error_reads_old_pairs() {
        let old: Vec<AccountError> = serde_json::from_str(r#"[["/h/.claude","Not logged in."]]"#).unwrap();
        assert_eq!(
            old[0],
            AccountError { dir: "/h/.claude".into(), message: "Not logged in.".into(), retrying: false }
        );
        let e = AccountError { dir: "/d".into(), message: "m".into(), retrying: true };
        let back: AccountError = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn snapshot_roundtrip() {
        let dir = std::env::temp_dir().join(format!("clawdmeter-ipc-{}", std::process::id()));
        let paths = Paths::under(&dir);
        let snap = StatusSnapshot::from_status(&status(connected(), true, Some(990)), 1_000);
        assert_eq!(snap.tray_state(), TrayState::Connected);
        assert_eq!((snap.session_pct, snap.week_pct), (Some(3), Some(12)));
        write_status(&paths, &snap).unwrap();
        let back = read_status(&paths).unwrap();
        assert_eq!(back, snap);
        assert!(back.alive(1_003));
        assert!(!back.alive(1_000 + STATUS_ALIVE_SECS + 1));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fable_rides_with_the_week() {
        let mut s = status(connected(), true, Some(990));
        let r = s.engine.reading.as_mut().unwrap();
        r.models = vec![
            ("Opus".into(), Window { pct: 9, resets_at: None }),
            ("FABLE".into(), Window { pct: 40, resets_at: None }),
        ];
        let snap = StatusSnapshot::from_status(&s, 1_000);
        assert_eq!(snap.fable_pct, Some(40));
        assert_eq!(snap.shown_fable(), Some(40), "drawn in the Weekly bar");
        // Never on a Codex account's bar or a spend-capped one's, whatever
        // the snapshot carries.
        assert_eq!(StatusSnapshot { codex: true, ..snap.clone() }.shown_fable(), None);
        assert_eq!(StatusSnapshot { spend: true, ..snap.clone() }.shown_fable(), None);
        // A status file from before the field reads as unknown.
        let mut old = serde_json::to_value(&snap).unwrap();
        old.as_object_mut().unwrap().remove("fable_pct");
        assert_eq!(serde_json::from_value::<StatusSnapshot>(old).unwrap().fable_pct, None);
        // A spend-capped account has no weekly meter.
        let r = s.engine.reading.as_mut().unwrap();
        (r.session, r.week) = (None, None);
        r.spend = Some(clawdmeter_core::usage::Spend { pct: 20, resets_at: None, period: None });
        assert_eq!(StatusSnapshot::from_status(&s, 1_000).fable_pct, None);
    }

    #[test]
    fn no_session_limit_has_no_session_meter() {
        let mut s = status(connected(), true, Some(990));
        let r = s.engine.reading.as_mut().unwrap();
        (r.session, r.session_limit) = (None, SessionLimit::Absent);
        let snap = StatusSnapshot::from_status(&s, 1_000);
        assert!(snap.no_session_limit);
        assert_eq!((snap.session_pct, snap.week_pct), (None, Some(12)));
        assert_eq!(snap.fable_pct, None, "no Fable window: no overlay");
        // A snapshot from before the field: an ordinary account.
        let mut old = serde_json::to_value(&snap).unwrap();
        old.as_object_mut().unwrap().remove("no_session_limit");
        assert!(!serde_json::from_value::<StatusSnapshot>(old).unwrap().no_session_limit);
    }

    #[test]
    fn commands_roundtrip_and_take() {
        let dir = std::env::temp_dir().join(format!("clawdmeter-cmd-{}", std::process::id()));
        let paths = Paths::under(&dir);
        let cmds = [
            AppCommand::Refresh,
            AppCommand::Open("hooks".into()),
            AppCommand::Forget,
            AppCommand::SettingsChanged,
            AppCommand::Quit,
        ];
        for c in &cmds {
            assert_eq!(AppCommand::decode(&c.encode()).as_ref(), Some(c));
            send_command(&paths, c).unwrap();
        }
        assert_eq!(AppCommand::decode("open nowhere"), None);
        assert_eq!(AppCommand::decode("rm -rf"), None);
        assert_eq!(take_commands(&paths), cmds.to_vec());
        assert!(take_commands(&paths).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn window_commands_and_lock() {
        let dir = std::env::temp_dir().join(format!("clawdmeter-win-{}", std::process::id()));
        let paths = Paths::under(&dir);
        send_window_command(&paths, crate::Page::Hooks).unwrap();
        send_window_command(&paths, crate::Page::Flash).unwrap();
        assert_eq!(take_window_commands(&paths), [crate::Page::Hooks, crate::Page::Flash]);
        assert!(take_window_commands(&paths).is_empty());
        let first = acquire_window_lock(&paths).unwrap();
        assert!(first.is_some());
        assert!(acquire_window_lock(&paths).unwrap().is_none());
        drop(first);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn instance_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("clawdmeter-lock-{}", std::process::id()));
        let paths = Paths::under(&dir);
        let first = acquire_instance_lock(&paths).unwrap();
        assert!(first.is_some());
        assert!(acquire_instance_lock(&paths).unwrap().is_none());
        drop(first);
        assert!(acquire_instance_lock(&paths).unwrap().is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn update_available_needs_a_known_device_version() {
        let mut s = StatusSnapshot { latest_firmware: Some("v1.9.0".into()), ..Default::default() };
        assert_eq!(s.update_available(), None);
        s.device_firmware = Some("1.8.2".into());
        assert_eq!(s.update_available(), Some("v1.9.0"));
        s.device_firmware = Some("1.9.0".into());
        assert_eq!(s.update_available(), None);
        s.device_firmware = Some("dev".into());
        assert_eq!(s.update_available(), None, "a local build is not offered a release");
    }

    #[test]
    fn app_update_is_a_newer_release_than_the_app() {
        let mut s = StatusSnapshot::default();
        assert_eq!(s.app_update("2.4.0"), None, "nothing checked yet");
        s.latest_firmware = Some("v2.5.0".into());
        assert_eq!(s.app_update("2.4.0").as_deref(), Some("v2.5.0"));
        assert_eq!(s.app_update("v2.5.0"), None);
        assert_eq!(s.app_update("2.6.0"), None);
        assert_eq!(s.app_update("2.5.0-rc.2").as_deref(), Some("v2.5.0"), "a release is newer than its candidates");
        s.latest_firmware = Some("2.5.0".into());
        assert_eq!(s.app_update("2.4.0").as_deref(), Some("v2.5.0"));
        s.latest_firmware = Some("9.0.0/../x".into());
        assert_eq!(s.app_update("2.4.0"), None, "not a version");
    }
}
