use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::Duration;

pub struct Config {
    /// Base URL of the screentime server. Without one the daemon runs on its cached policy.
    pub server: Option<String>,
    pub token: Option<String>,
    /// Extra CA certificate (PEM) to trust for the server, e.g. for a self-signed one.
    pub ca_file: Option<PathBuf>,
    /// SHA-256 of the server's certificate. When set, only that certificate is accepted,
    /// which lets a self-signed server work without installing a CA.
    pub server_fingerprint: Option<[u8; 32]>,
    /// Local account names to limit.
    pub users: Vec<String>,
    /// Local account name -> the name the server knows them by, when they differ
    /// (`SCREENTIME_USERS=ryany=ryan`), so one kid has one schedule across computers.
    pub server_names: std::collections::HashMap<String, String>,
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    /// How long a session stays locked after time runs out before it is signed out, when
    /// `sign_out` is on.
    pub grace: Duration,
    /// Sign kids out once the grace period is over. Off by default: their sessions stay locked,
    /// with everything still open, and are locked again whenever they're unlocked.
    pub sign_out: bool,
    pub host: String,
    /// Members of this group can approve time on the spot with "My parent is here".
    pub admin_group: String,
    pub pam_service: String,
    /// Replaces /etc/pam.d for the parent check; only for tests.
    pub pam_confdir: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());

        let (users, server_names) = parse_users(&var("SCREENTIME_USERS").unwrap_or_default());
        if users.iter().any(|u| u == "root" || u.eq_ignore_ascii_case("administrator")) {
            bail!("an administrator account cannot be a managed user");
        }

        let token = match var("SCREENTIME_TOKEN_FILE") {
            Some(path) => Some(
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading token file {path}"))?
                    .trim()
                    .to_owned(),
            ),
            None => None,
        };

        let grace_secs = match var("SCREENTIME_GRACE_SECS") {
            Some(s) => s.parse().context("SCREENTIME_GRACE_SECS")?,
            None => 120,
        };

        Ok(Config {
            server: var("SCREENTIME_SERVER").map(|s| s.trim_end_matches('/').to_owned()),
            token,
            ca_file: var("SCREENTIME_CA_FILE").map(PathBuf::from),
            server_fingerprint: match var("SCREENTIME_SERVER_FINGERPRINT") {
                Some(f) => Some(
                    crate::pinning::parse_fingerprint(&f)
                        .context("SCREENTIME_SERVER_FINGERPRINT must be a SHA-256 fingerprint")?,
                ),
                None => None,
            },
            users,
            server_names,
            state_dir: var("SCREENTIME_STATE_DIR").unwrap_or_else(|| DEFAULT_STATE_DIR.into()).into(),
            socket: var("SCREENTIME_SOCKET").unwrap_or_else(|| DEFAULT_SOCKET.into()).into(),
            grace: Duration::from_secs(grace_secs),
            sign_out: matches!(var("SCREENTIME_SIGN_OUT").as_deref(), Some("1" | "true" | "yes" | "on")),
            host: hostname(),
            admin_group: var("SCREENTIME_ADMIN_GROUP").unwrap_or_else(|| DEFAULT_ADMIN_GROUP.into()),
            pam_service: var("SCREENTIME_PAM_SERVICE").unwrap_or_else(|| "screentime".into()),
            pam_confdir: var("SCREENTIME_PAM_CONFDIR"),
        })
    }

    /// Windows user names are case-insensitive; Linux ones aren't.
    pub fn is_managed(&self, user: &str) -> bool {
        self.users.iter().any(|u| if cfg!(windows) { u.eq_ignore_ascii_case(user) } else { u == user })
    }

    /// The name the server knows a (configured) local account by.
    pub fn server_name<'a>(&'a self, local: &'a str) -> &'a str {
        self.server_names.get(local).map(String::as_str).unwrap_or(local)
    }

    /// The configured spelling of `user`, so state is keyed the same however Windows reports it.
    pub fn managed_name(&self, user: &str) -> Option<&str> {
        self.users
            .iter()
            .find(|u| if cfg!(windows) { u.eq_ignore_ascii_case(user) } else { *u == user })
            .map(String::as_str)
    }
}

/// `a,b=c` -> local accounts `[a, b]`, with `b` known to the server as `c`.
fn parse_users(list: &str) -> (Vec<String>, std::collections::HashMap<String, String>) {
    let mut users = Vec::new();
    let mut server_names = std::collections::HashMap::new();
    for entry in list.split(',').map(str::trim) {
        match entry.split_once('=') {
            Some((local, server)) if !local.trim().is_empty() && !server.trim().is_empty() => {
                users.push(local.trim().to_owned());
                server_names.insert(local.trim().to_owned(), server.trim().to_owned());
            }
            None if !entry.is_empty() => users.push(entry.to_owned()),
            _ => {}
        }
    }
    (users, server_names)
}

#[cfg(unix)]
const DEFAULT_STATE_DIR: &str = "/var/lib/screentime";
#[cfg(windows)]
const DEFAULT_STATE_DIR: &str = r"C:\ProgramData\screentime";
#[cfg(unix)]
const DEFAULT_SOCKET: &str = screentime_common::CLIENT_SOCKET;
#[cfg(windows)]
const DEFAULT_SOCKET: &str = screentime_common::CLIENT_PIPE;
#[cfg(unix)]
const DEFAULT_ADMIN_GROUP: &str = "wheel";
#[cfg(windows)]
const DEFAULT_ADMIN_GROUP: &str = "Administrators";

fn hostname() -> String {
    #[cfg(target_os = "linux")]
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok();
    // macOS: the Bonjour name ("m1" for m1.local), which is what people call the Mac.
    #[cfg(target_os = "macos")]
    let name = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok());
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    name.map(|h| h.trim().to_owned()).filter(|h| !h.is_empty()).unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn users_with_server_names() {
        let (users, names) = super::parse_users(" sam , ryany = ryan ,, =x");
        assert_eq!(users, ["sam", "ryany"]);
        assert_eq!(names.get("ryany").map(String::as_str), Some("ryan"));
        assert!(!names.contains_key("sam"));
    }
}
