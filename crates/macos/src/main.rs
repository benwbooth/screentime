//! The macOS client: the shared screen time agent, with sessions read from the IORegistry and
//! locking through loginwindow (the real lock screen; nothing is ever signed out).
//!
//! It runs either as a root LaunchDaemon (installed with sudo) or, when the limited account
//! is an administrator anyway, as a LaunchAgent in that account's own session.
//!
//! `screentime-mac --lock` locks the session it runs in and exits.

#[cfg(target_os = "macos")]
mod mac {
    use anyhow::{Context, Result, bail};
    use screentime_client::agent::{Agent, Platform, Session};
    use screentime_client::config::Config;
    use screentime_common::LocalAccount;
    use std::ffi::CString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixListener;

    /// How often the console is checked between ticks, so an unlock while out of time is
    /// locked again within a couple of seconds rather than at the next 15-second tick.
    const WATCH: Duration = Duration::from_secs(2);

    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    fn uid_of(user: &str) -> Option<u32> {
        let name = CString::new(user).ok()?;
        let pw = unsafe { libc::getpwnam(name.as_ptr()) };
        (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
    }

    fn home() -> PathBuf {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/var/root".into())
    }

    /// Where settings, usage and the socket live.
    fn data_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("SCREENTIME_STATE_DIR") {
            return dir.into();
        }
        if is_root() { "/Library/Application Support/screentime".into() } else { home().join("Library/Application Support/screentime") }
    }

    /// Lock the GUI session this process belongs to, as if Lock Screen were chosen from the menu.
    pub fn lock_this_session() -> Result<()> {
        // SACLockScreenImmediate is what the Apple menu's Lock Screen uses. It is private, so
        // look it up at run time and fall back to sleeping the display (which locks when "require
        // password after sleep" is immediate, the default).
        type LockFn = unsafe extern "C" fn() -> i32;
        let path = c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login";
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW) };
        if !handle.is_null() {
            let sym = unsafe { libc::dlsym(handle, c"SACLockScreenImmediate".as_ptr()) };
            if !sym.is_null() {
                let lock: LockFn = unsafe { std::mem::transmute(sym) };
                let rc = unsafe { lock() };
                if rc == 0 {
                    return Ok(());
                }
                tracing::warn!(rc, "SACLockScreenImmediate failed, sleeping the display instead");
            }
        }
        let status = Command::new("/usr/bin/pmset").arg("displaysleepnow").status()?;
        if !status.success() {
            bail!("pmset displaysleepnow failed");
        }
        Ok(())
    }

    #[derive(serde::Deserialize)]
    struct ConsoleUser {
        #[serde(rename = "kCGSSessionUserNameKey")]
        user: Option<String>,
        #[serde(rename = "kCGSSessionIDKey")]
        id: Option<u64>,
        #[serde(rename = "kCGSSessionOnConsoleKey", default)]
        on_console: bool,
        #[serde(rename = "kCGSessionLoginDoneKey", default)]
        login_done: bool,
        #[serde(rename = "CGSSessionScreenIsLocked", default)]
        locked: bool,
    }

    /// GUI sessions, from the IORegistry's IOConsoleUsers (readable by anyone; root not needed).
    /// A session switched away from with fast user switching is neither locked nor in use.
    fn console_sessions() -> Result<Vec<Session>> {
        let out = Command::new("/bin/sh")
            .args(["-c", "/usr/sbin/ioreg -n Root -d1 -a | /usr/bin/plutil -extract IOConsoleUsers json -o - -"])
            .output()
            .context("running ioreg")?;
        if !out.status.success() {
            // No IOConsoleUsers at all: nobody signed in.
            return Ok(Vec::new());
        }
        let users: Vec<ConsoleUser> = serde_json::from_slice(&out.stdout).context("parsing IOConsoleUsers")?;
        Ok(users
            .into_iter()
            .filter_map(|u| {
                let user = u.user.filter(|n| !n.is_empty() && n != "loginwindow")?;
                Some(Session {
                    id: u.id.unwrap_or(0).to_string(),
                    in_use: u.on_console && u.login_done && !u.locked,
                    locked: u.locked,
                    user,
                })
            })
            .collect())
    }

    fn group_members(group: &str) -> Vec<String> {
        let out = Command::new("/usr/bin/dscl").args([".", "-read", &format!("/Groups/{group}"), "GroupMembership"]).output();
        let Ok(out) = out else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect()
    }

    struct Mac;

    impl Platform for Mac {
        async fn sessions(&self) -> Result<Vec<Session>> {
            tokio::task::spawn_blocking(console_sessions).await?
        }

        async fn lock(&self, session: &Session) -> Result<()> {
            let uid = uid_of(&session.user).context("no such user")?;
            if unsafe { libc::geteuid() } == uid {
                return tokio::task::spawn_blocking(lock_this_session).await?;
            }
            if !is_root() {
                bail!("can't lock {}'s session from {}'s agent", session.user, unsafe { libc::geteuid() });
            }
            // Run ourselves with --lock inside that user's GUI session.
            let exe = std::env::current_exe()?;
            let status = tokio::process::Command::new("/bin/launchctl")
                .args(["asuser", &uid.to_string(), "/usr/bin/sudo", "-u", &session.user])
                .arg(exe)
                .arg("--lock")
                .status()
                .await?;
            if !status.success() {
                bail!("locking via launchctl asuser failed: {status}");
            }
            Ok(())
        }

        async fn end_sessions(&self, user: &str, _sessions: &[Session]) -> Result<()> {
            // Never signed out on a Mac: the session stays locked and is locked again on unlock.
            tracing::info!(user, "signing out isn't done on macOS; the session stays locked");
            Ok(())
        }

        fn local_accounts(&self) -> Vec<LocalAccount> {
            let admins = group_members("admin");
            let Ok(out) = Command::new("/usr/bin/dscl").args([".", "-list", "/Users", "UniqueID"]).output() else {
                return Vec::new();
            };
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| {
                    let mut parts = l.split_whitespace();
                    let (name, uid) = (parts.next()?, parts.next()?.parse::<i64>().ok()?);
                    (uid >= 500 && !name.starts_with('_')).then(|| LocalAccount {
                        admin: admins.iter().any(|a| a == name),
                        name: name.to_owned(),
                        configured_kid: None,
                    })
                })
                .collect()
        }

        fn admins(&self) -> Vec<String> {
            group_members("admin").into_iter().filter(|u| u != "root").collect()
        }
    }

    /// KEY=VALUE lines from config.env in the data directory, without overriding the environment.
    fn load_config_file(dir: &Path) -> Result<()> {
        let path = dir.join("config.env");
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if let Some((k, v)) = line.split_once('=')
                && std::env::var_os(k.trim()).is_none()
            {
                unsafe { std::env::set_var(k.trim(), v.trim()) };
            }
        }
        Ok(())
    }

    fn bind_socket(path: &Path) -> Result<UnixListener> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
        // As root, the menu-bar app in the kid's session must reach it; as a user agent, only
        // that user.
        let mode = if is_root() { 0o666 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        Ok(listener)
    }

    pub async fn main() -> Result<()> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        load_config_file(&dir)?;
        if std::env::var_os("SCREENTIME_STATE_DIR").is_none() {
            unsafe { std::env::set_var("SCREENTIME_STATE_DIR", &dir) };
        }
        if std::env::var_os("SCREENTIME_SOCKET").is_none() {
            unsafe { std::env::set_var("SCREENTIME_SOCKET", dir.join("client.sock")) };
        }
        let config = Config::from_env()?;
        let listener = bind_socket(&config.socket)?;
        let agent = Agent::new(config, Mac)?;

        let server = agent.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let agent = server.clone();
                        tokio::spawn(async move {
                            let (r, w) = stream.into_split();
                            if let Err(e) = agent.handle_connection(r, w).await {
                                tracing::debug!("socket connection ended: {e:#}");
                            }
                        });
                    }
                    Err(e) => tracing::error!("accept failed: {e:#}"),
                }
            }
        });

        // Tick early whenever someone signs in, unlocks or switches users.
        let watched = agent.clone();
        tokio::spawn(async move {
            let mut last: Option<Vec<(String, String, bool)>> = None;
            loop {
                tokio::time::sleep(WATCH).await;
                let Ok(Ok(sessions)) = tokio::task::spawn_blocking(console_sessions).await else { continue };
                let now: Vec<_> = sessions.into_iter().map(|s| (s.id, s.user, s.in_use)).collect();
                if last.as_ref().is_some_and(|l| *l != now) {
                    watched.wake();
                }
                last = Some(now);
            }
        });

        Arc::clone(&agent).run().await;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
    if std::env::args().any(|a| a == "--lock") {
        return mac::lock_this_session();
    }
    mac::main().await
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("screentime-mac only runs on macOS.");
    std::process::exit(1);
}
