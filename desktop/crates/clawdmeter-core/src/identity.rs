//! Which Claude account a config dir is signed in to.
//!
//! The hub (`crate::remote`) gets hook records from other computers, whose
//! config dirs are named whatever that computer names them: dev's `~/.claude`
//! can be another account than this computer's `~/.claude`. Records are
//! therefore matched to a local account by who is signed in, never by path.
//!
//! The account id (`accountUuid`) is in `<dir>/.credentials.json`
//! (`claudeAiOauth`, Linux and Windows) and in `.claude.json`
//! (`oauthAccount`, with the email; on macOS the credentials sit in the
//! Keychain, so only this one is on disk). For the default dir (`~/.claude`)
//! that file is `~/.claude.json`, beside the dir, as for
//! `usage::for_account`.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Who an account is signed in as. Any part may be unknown. One login can
/// belong to several organizations (a personal plan and a team), each with
/// its own limits, so the organization is part of the account.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    /// `accountUuid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// `emailAddress`. A sender drops it when it has the id ([`Identity::for_wire`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// `organizationUuid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
}

impl Identity {
    /// Neither part is known.
    pub fn is_empty(&self) -> bool {
        self.uuid.is_none() && self.email.is_none()
    }

    /// The same account: the ids match when both sides have one, else the
    /// emails (ignoring case); and the organizations match when both sides
    /// have one.
    pub fn same(&self, other: &Identity) -> bool {
        let who = match (&self.uuid, &other.uuid) {
            (Some(a), Some(b)) => a == b,
            _ => matches!((&self.email, &other.email), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b)),
        };
        let org = match (&self.org, &other.org) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        who && org
    }

    /// What a sender puts on the network: the ids, and the email only when
    /// there is no id (a `hub_accounts` hint).
    pub fn for_wire(mut self) -> Identity {
        if self.uuid.is_some() {
            self.email = None;
        }
        self
    }

    /// `dir=email` and `dir=uuid` entries name an account either way: an
    /// `@` makes it an email.
    pub fn from_hint(hint: &str) -> Identity {
        let hint = hint.trim();
        if hint.is_empty() {
            Identity::default()
        } else if hint.contains('@') {
            Identity { email: Some(hint.to_owned()), ..Default::default() }
        } else {
            Identity { uuid: Some(hint.to_owned()), ..Default::default() }
        }
    }
}

/// The `.claude.json` that belongs to `config_dir`.
pub fn claude_json(config_dir: &Path, home: &Path) -> PathBuf {
    if config_dir == home.join(".claude") {
        home.join(".claude.json")
    } else {
        config_dir.join(".claude.json")
    }
}

fn text(v: &Value, outer: &str, key: &str) -> Option<String> {
    v.get(outer)?.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Read who `config_dir` is signed in as: the credentials file's account and
/// organization ids (a small file, so the hook stays fast), else
/// `.claude.json`'s ids and email. `None` when neither file says.
pub fn read(config_dir: &Path, home: &Path) -> Option<Identity> {
    let json = |p: PathBuf| std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let from = |v: &Value, outer: &str| Identity {
        uuid: text(v, outer, "accountUuid"),
        email: text(v, outer, "emailAddress"),
        org: text(v, outer, "organizationUuid"),
    };
    if let Some(id) = json(config_dir.join(".credentials.json")).map(|v| from(&v, "claudeAiOauth")) {
        if id.uuid.is_some() {
            return Some(id);
        }
    }
    let id = from(&json(claude_json(config_dir, home))?, "oauthAccount");
    (!id.is_empty()).then_some(id)
}

/// The email `config_dir` is signed in with (`.claude.json`'s
/// `oauthAccount`; the credentials file has none), for its short name.
pub fn email(config_dir: &Path, home: &Path) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(claude_json(config_dir, home)).ok()?).ok()?;
    text(&v, "oauthAccount", "emailAddress")
}

/// A short name for each account, for the rows that list them all: the
/// sign-in's email domain ("example.com") when no other account has the same
/// one, else `None`, and the caller shows the folder (two accounts on one
/// domain, or one login in two organizations, still read apart).
pub fn labels(emails: &[Option<String>]) -> Vec<Option<String>> {
    let domains: Vec<Option<String>> = emails
        .iter()
        .map(|e| e.as_deref().map(|e| e.rsplit_once('@').map_or(e, |(_, d)| d).to_ascii_lowercase()))
        .collect();
    domains
        .iter()
        .map(|d| d.clone().filter(|d| domains.iter().filter(|o| o.as_deref() == Some(d.as_str())).count() == 1))
        .collect()
}

/// This computer's accounts and who each is signed in as.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityMap {
    entries: Vec<(PathBuf, Identity)>,
}

impl IdentityMap {
    /// Read every dir's identity. Dirs nobody is signed in to are left out.
    pub fn build(dirs: &[PathBuf], home: &Path) -> Self {
        IdentityMap { entries: dirs.iter().filter_map(|d| Some((d.clone(), read(d, home)?))).collect() }
    }

    /// The local dir signed in as `id` (the first, if several are).
    pub fn lookup(&self, id: &Identity) -> Option<&Path> {
        self.entries.iter().find(|(_, mine)| mine.same(id)).map(|(d, _)| d.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn reads_who_is_signed_in() {
        let root = std::env::temp_dir().join(format!("cm-identity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let put = |rel: &str, body: &str| {
            let p = home.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        };
        put(
            ".claude/.credentials.json",
            r#"{"claudeAiOauth":{"accessToken":"x","accountUuid":"u-a","organizationUuid":"o-1"}}"#,
        );
        put(".claude.json", r#"{"oauthAccount":{"accountUuid":"u-a","emailAddress":"a@x.test"}}"#);
        put(".claude-b/.credentials.json", r#"{"mcpOAuth":{}}"#);
        put(
            ".claude-b/.claude.json",
            r#"{"oauthAccount":{"accountUuid":"u-b","emailAddress":"b@x.test","organizationUuid":"o-1"}}"#,
        );
        // The same login in another organization (a team plan).
        put(".claude-t/.claude.json", r#"{"oauthAccount":{"accountUuid":"u-b","organizationUuid":"o-team"}}"#);
        put(".claude-c/.claude.json", r#"{"numStartups":3}"#);
        let id = |uuid: Option<&str>, email: Option<&str>, org: Option<&str>| Identity {
            uuid: uuid.map(str::to_owned),
            email: email.map(str::to_owned),
            org: org.map(str::to_owned),
        };
        // (dir, what read() finds)
        let cases = [
            (".claude", Some(id(Some("u-a"), None, Some("o-1")))),
            (".claude-b", Some(id(Some("u-b"), Some("b@x.test"), Some("o-1")))),
            (".claude-t", Some(id(Some("u-b"), None, Some("o-team")))),
            (".claude-c", None),
            (".claude-d", None),
        ];
        for (dir, want) in &cases {
            assert_eq!(read(&home.join(dir), &home), *want, "{dir}");
        }
        let dirs: Vec<PathBuf> = cases.iter().map(|(d, _)| home.join(d)).collect();
        let map = IdentityMap::build(&dirs, &home);
        // (who a record says it is, the local dir it maps to)
        let lookups = [
            (id(Some("u-b"), None, Some("o-1")), Some(".claude-b")),
            (id(Some("u-b"), None, Some("o-team")), Some(".claude-t")),
            (id(Some("u-b"), None, Some("o-other")), None),
            (id(Some("u-a"), None, None), Some(".claude")),
            (id(Some("u-z"), None, None), None),
            (Identity::default(), None),
        ];
        // Short names: the email domain while no other account shares it.
        let mail = |e: &str| Some(e.to_owned());
        let names = labels(&[mail("me@Acme.ai"), mail("a@x.test"), mail("b@x.test"), None]);
        assert_eq!(names, [Some("acme.ai".into()), None, None, None]);
        assert_eq!(
            email(&home.join(".claude"), &home).as_deref(),
            Some("a@x.test"),
            "the default dir's ~/.claude.json"
        );
        // A hub_accounts hint names an account by email or id.
        assert_eq!(map.lookup(&Identity::from_hint("B@X.test")), Some(home.join(".claude-b").as_path()));
        assert_eq!(map.lookup(&Identity::from_hint("u-z")), None);
        for (who, want) in lookups {
            assert_eq!(map.lookup(&who), want.map(|d| home.join(d)).as_deref(), "{who:?}");
        }
        let _ = fs::remove_dir_all(&root);
    }
}
