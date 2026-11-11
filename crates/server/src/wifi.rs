//! Wi-Fi time. Wi-Fi is a built-in user ("wifi", shown as "Wi-Fi") with everything a kid has:
//! a daily limit, downtime, time added or taken away, a lock, and history. Minutes count while
//! the Wi-Fi is on; when the Wi-Fi user is out of time, in downtime or locked, the server turns
//! the router's Wi-Fi off, and turns it back on when it's allowed again.
//!
//! Nothing touches the router until a parent has saved the router password and turned Wi-Fi
//! time on, so a fresh install (or an upgrade) never cuts the Wi-Fi by itself.

use crate::{AppState, db, script};
use anyhow::{Context, Result};
use chrono::{Local, Utc};
use screentime_common::{Decision, UsageReport, evaluate};
use serde::Serialize;
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};

/// The built-in user that stands for the Wi-Fi.
pub const USER: &str = "wifi";
/// The "computer" its minutes are recorded under.
pub const HOST: &str = "router";

/// How often the controller looks at the schedule even when nothing woke it.
const TICK: Duration = Duration::from_secs(15);
/// Wait before trying the router again after it failed.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// After a refused login, wait longer: the router locks the admin out after repeated tries.
const RETRY_AFTER_BAD_PASSWORD: Duration = Duration::from_secs(15 * 60);

/// What the server last did to the router, for the UI.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RouterStatus {
    /// What the server last set the Wi-Fi to; `None` until it has since starting.
    pub on: Option<bool>,
    pub last_changed_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
}

#[derive(Default)]
pub struct Wifi {
    settings: Mutex<()>,
    /// Held while a router script runs.
    router_busy: Mutex<()>,
    router: Mutex<RouterStatus>,
    wake: Notify,
    /// Settings changed: try the router again now instead of waiting out a back-off.
    retry_now: AtomicBool,
}

impl Wifi {
    pub async fn router_status(&self) -> RouterStatus {
        self.router.lock().await.clone()
    }

    /// Look at the schedule again now, e.g. after time was added or a lock changed.
    pub fn poke(&self) {
        self.wake.notify_one();
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Row {
    pub enabled: bool,
    pub router_url: String,
    pub router_user: String,
    pub router_password: Option<String>,
    pub script_on: Option<String>,
    pub script_off: Option<String>,
    pub script_status: Option<String>,
}

impl Row {
    pub fn password_set(&self) -> bool {
        self.router_password.as_deref().is_some_and(|p| !p.is_empty())
    }

    /// The script that does `which`, the built-in one unless it was changed.
    pub fn script(&self, which: Script) -> &str {
        let (saved, default) = match which {
            Script::On => (&self.script_on, script::DEFAULT_ON),
            Script::Off => (&self.script_off, script::DEFAULT_OFF),
            Script::Status => (&self.script_status, script::DEFAULT_STATUS),
        };
        saved.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or(default)
    }

    pub fn router_config(&self) -> script::RouterConfig {
        script::RouterConfig {
            url: self.router_url.clone(),
            user: self.router_user.clone(),
            password: self.router_password.clone().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Script {
    On,
    Off,
    Status,
}

/// Run one of the router scripts, one at a time (the router allows a single admin session).
pub async fn run_script(state: &AppState, row: &Row, which: Script, code: Option<String>) -> script::Outcome {
    let _one_at_a_time = state.wifi.router_busy.lock().await;
    let code = code.unwrap_or_else(|| row.script(which).to_owned());
    let outcome = script::run(code, row.router_config()).await;
    if !outcome.ok {
        tracing::warn!(script = ?which, "router script failed: {}", outcome.message.as_deref().unwrap_or(""));
    }
    outcome
}

pub async fn load(db: &SqlitePool) -> Result<Row> {
    Ok(sqlx::query_as(
        "SELECT enabled, router_url, router_user, router_password, script_on, script_off, script_status FROM wifi WHERE id = 1",
    )
        .fetch_one(db)
        .await?)
}

pub async fn user_id(db: &SqlitePool) -> Result<i64> {
    db::user_id(db, USER).await?.context("the built-in Wi-Fi user is missing")
}

/// Whether the Wi-Fi may be on right now, by its schedule, time and lock.
pub async fn decision(db: &SqlitePool) -> Result<Decision> {
    let id = user_id(db).await?;
    let policy = db::policy_for(db, id, USER, "", Local::now().date_naive()).await?;
    Ok(evaluate(Some(&policy), Local::now().naive_local(), 0))
}

/// Count `minutes` more of Wi-Fi use today.
async fn record_use(db: &SqlitePool, minutes: u32) -> Result<()> {
    let id = user_id(db).await?;
    let day = Local::now().date_naive();
    let so_far = db::usage(db, id, day).await?.into_iter().find(|u| u.host == HOST).map_or(0, |u| u.active_minutes);
    let report = UsageReport {
        username: USER.into(),
        host: HOST.into(),
        day,
        active_minutes: so_far + minutes,
        local_bonus_minutes: 0,
    };
    db::record_usage(db, id, HOST, &report).await
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct SettingsChange {
    pub enabled: Option<bool>,
    pub router_url: Option<String>,
    pub router_user: Option<String>,
    /// A new password; an empty string clears it. Left out keeps the saved one.
    pub router_password: Option<String>,
    /// New scripts; an empty string goes back to the built-in one.
    pub script_on: Option<String>,
    pub script_off: Option<String>,
    pub script_status: Option<String>,
}

/// `http://192.168.1.1` from `192.168.1.1`, `http://192.168.1.1/`, etc. Only a scheme and host.
pub fn normalize_router_url(input: &str) -> Option<String> {
    let input = input.trim().trim_end_matches('/');
    let with_scheme = if input.contains("://") { input.to_owned() } else { format!("http://{input}") };
    let url = reqwest::Url::parse(&with_scheme).ok()?;
    let ok = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|h| !h.is_empty())
        && url.path() == "/"
        && url.query().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    ok.then_some(with_scheme)
}

pub enum SettingsError {
    Invalid(String),
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for SettingsError {
    fn from(e: E) -> Self {
        SettingsError::Other(e.into())
    }
}

pub async fn change_settings(state: &AppState, change: SettingsChange) -> Result<(), SettingsError> {
    let _guard = state.wifi.settings.lock().await;
    let mut row = load(&state.db).await?;
    if let Some(url) = &change.router_url {
        row.router_url = normalize_router_url(url)
            .ok_or_else(|| SettingsError::Invalid("Enter the router's address, like 192.168.1.1.".into()))?;
    }
    if let Some(user) = &change.router_user {
        let user = user.trim();
        if user.is_empty() || user.chars().count() > 64 {
            return Err(SettingsError::Invalid("Enter the router's admin username.".into()));
        }
        row.router_user = user.to_owned();
    }
    if let Some(password) = change.router_password {
        if password.chars().count() > 128 {
            return Err(SettingsError::Invalid("That password is too long.".into()));
        }
        row.router_password = (!password.is_empty()).then_some(password);
    }
    for (new, saved, default) in [
        (change.script_on, &mut row.script_on, script::DEFAULT_ON),
        (change.script_off, &mut row.script_off, script::DEFAULT_OFF),
        (change.script_status, &mut row.script_status, script::DEFAULT_STATUS),
    ] {
        if let Some(code) = new {
            if code.len() > script::MAX_SCRIPT_BYTES {
                return Err(SettingsError::Invalid("That script is too long.".into()));
            }
            // Saving the built-in script unchanged keeps following future updates to it.
            *saved = (!code.trim().is_empty() && code.trim() != default.trim()).then_some(code);
        }
    }
    if let Some(enabled) = change.enabled {
        row.enabled = enabled;
    }
    if row.enabled && !row.password_set() {
        return Err(SettingsError::Invalid("Save the router password first.".into()));
    }
    sqlx::query(
        "UPDATE wifi SET enabled = ?, router_url = ?, router_user = ?, router_password = ?,
         script_on = ?, script_off = ?, script_status = ? WHERE id = 1",
    )
    .bind(row.enabled)
    .bind(&row.router_url)
    .bind(&row.router_user)
    .bind(&row.router_password)
    .bind(&row.script_on)
    .bind(&row.script_off)
    .bind(&row.script_status)
        .execute(&state.db)
        .await?;
    drop(_guard);
    tracing::info!(enabled = row.enabled, router = %row.router_url, "Wi-Fi settings changed");
    state.wifi.retry_now.store(true, Ordering::SeqCst);
    state.wifi.poke();
    Ok(())
}

/// Run a script from the settings page, saved or as edited. A successful on/off test or a status
/// check also tells the server what the router is doing now.
pub async fn test_script(state: &AppState, which: Script, code: Option<String>) -> Result<script::Outcome> {
    let row = load(&state.db).await?;
    if let Some(code) = &code
        && code.len() > script::MAX_SCRIPT_BYTES
    {
        return Ok(script::Outcome { message: Some("That script is too long.".into()), ..Default::default() });
    }
    let outcome = run_script(state, &row, which, code).await;
    let known = match (which, outcome.ok, &outcome.value) {
        (Script::On, true, _) => Some(true),
        (Script::Off, true, _) => Some(false),
        (Script::Status, true, serde_json::Value::Bool(b)) => Some(*b),
        _ => None,
    };
    if let Some(on) = known {
        let mut status = state.wifi.router.lock().await;
        if status.on != Some(on) {
            status.last_changed_at = Some(Utc::now().to_rfc3339());
        }
        status.on = Some(on);
        status.last_error = None;
        status.last_error_at = None;
        drop(status);
        // The schedule may want it the other way; let the controller look again.
        state.wifi.poke();
    }
    Ok(outcome)
}

/// The scripts as saved, and the built-in ones for "Reset to default".
#[derive(Debug, Serialize)]
pub struct Scripts {
    pub on: String,
    pub off: String,
    pub status: String,
    pub default_on: &'static str,
    pub default_off: &'static str,
    pub default_status: &'static str,
}

pub async fn scripts(db: &SqlitePool) -> Result<Scripts> {
    let row = load(db).await?;
    Ok(Scripts {
        on: row.script(Script::On).to_owned(),
        off: row.script(Script::Off).to_owned(),
        status: row.script(Script::Status).to_owned(),
        default_on: script::DEFAULT_ON,
        default_off: script::DEFAULT_OFF,
        default_status: script::DEFAULT_STATUS,
    })
}

/// The router side of Wi-Fi time, for the UI. Never includes the router password.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    /// The built-in user's username, for its schedule, time and history.
    pub username: &'static str,
    /// Wi-Fi time switches the router.
    pub enabled: bool,
    pub password_set: bool,
    pub router_url: String,
    pub router_user: String,
    /// What the router should be set to; `None` when the server leaves it alone.
    pub wanted_on: Option<bool>,
    pub router: RouterStatus,
    /// Which scripts were changed from the built-in ones.
    pub custom_scripts: Vec<Script>,
}

pub async fn overview(state: &AppState) -> Result<Overview> {
    let row = load(&state.db).await?;
    let router = state.wifi.router_status().await;
    let allowed = matches!(decision(&state.db).await?, Decision::Allowed { .. });
    Ok(Overview {
        username: USER,
        enabled: row.enabled,
        password_set: row.password_set(),
        wanted_on: wanted(&row, allowed, router.on),
        custom_scripts: [
            (Script::On, &row.script_on),
            (Script::Off, &row.script_off),
            (Script::Status, &row.script_status),
        ]
        .into_iter()
        .filter(|(_, s)| s.is_some())
        .map(|(w, _)| w)
        .collect(),
        router_url: row.router_url,
        router_user: row.router_user,
        router,
    })
}

/// What the router should be: on while the Wi-Fi user is allowed, off otherwise. When Wi-Fi
/// time is turned off, the server's own "off" gets undone; otherwise it leaves the router alone.
fn wanted(row: &Row, allowed: bool, router_on: Option<bool>) -> Option<bool> {
    if !row.password_set() {
        return None;
    }
    if row.enabled {
        Some(allowed)
    } else if router_on == Some(false) {
        Some(true)
    } else {
        None
    }
}

/// Get the router to `on`. When the server doesn't know what the router is doing yet (it just
/// started), ask the status script first, so a restart doesn't re-apply a setting that's already
/// there (applying restarts the radios).
async fn switch(state: &AppState, row: &Row, on: bool, known: Option<bool>) -> Result<(), script::Outcome> {
    if known.is_none() {
        let status = run_script(state, row, Script::Status, None).await;
        if status.ok && status.value == serde_json::Value::Bool(on) {
            return Ok(());
        }
        // A refused password won't work any better for the switch; anything else, just try it.
        if !status.ok && status.bad_password {
            return Err(status);
        }
    }
    let outcome = run_script(state, row, if on { Script::On } else { Script::Off }, None).await;
    if outcome.ok { Ok(()) } else { Err(outcome) }
}

/// Keep the router in line with the schedule, and count the minutes the Wi-Fi is on. Runs for
/// the life of the server: once at start, then whenever something changes, and every TICK.
pub async fn run(state: AppState) {
    let mut retry_at: Option<Instant> = None;
    let mut last = Instant::now();
    // Time on not yet recorded as a whole minute.
    let mut unrecorded = Duration::ZERO;
    loop {
        if state.wifi.retry_now.swap(false, Ordering::SeqCst) {
            retry_at = None;
        }
        let now = Instant::now();
        let elapsed = now - last;
        last = now;
        let mut sleep_for = TICK;
        match load(&state.db).await {
            Ok(row) => {
                let current = state.wifi.router.lock().await.on;
                // Minutes count while Wi-Fi time is on and the Wi-Fi is on.
                if row.enabled && row.password_set() && current == Some(true) {
                    unrecorded += elapsed;
                    let minutes = (unrecorded.as_secs() / 60) as u32;
                    if minutes > 0 {
                        unrecorded -= Duration::from_secs(minutes as u64 * 60);
                        if let Err(e) = record_use(&state.db, minutes).await {
                            tracing::warn!("recording Wi-Fi use failed: {e:#}");
                        }
                    }
                    sleep_for = sleep_for.min(Duration::from_secs(60) - unrecorded);
                } else {
                    unrecorded = Duration::ZERO;
                }
                let allowed = match decision(&state.db).await {
                    Ok(d) => matches!(d, Decision::Allowed { .. }),
                    Err(e) => {
                        tracing::warn!("checking the Wi-Fi schedule failed: {e:#}");
                        // Leave the router as it is.
                        current.unwrap_or(true)
                    }
                };
                if let Some(on) = wanted(&row, allowed, current)
                    && current != Some(on)
                    && retry_at.is_none_or(|t| Instant::now() >= t)
                {
                    tracing::info!(on, "switching the Wi-Fi");
                    let result = switch(&state, &row, on, current).await;
                    let now = Utc::now().to_rfc3339();
                    let mut status = state.wifi.router.lock().await;
                    match result {
                        Ok(()) => {
                            tracing::info!(on, "Wi-Fi switched");
                            if status.on != Some(on) {
                                status.last_changed_at = Some(now);
                            }
                            status.on = Some(on);
                            status.last_error = None;
                            status.last_error_at = None;
                            retry_at = None;
                        }
                        Err(e) => {
                            let message = e.message.unwrap_or_else(|| "the router script failed".into());
                            tracing::warn!(on, "switching the Wi-Fi failed: {message}");
                            status.last_error = Some(message);
                            status.last_error_at = Some(now);
                            let wait = if e.bad_password { RETRY_AFTER_BAD_PASSWORD } else { RETRY_AFTER };
                            retry_at = Some(Instant::now() + wait);
                        }
                    }
                    continue;
                }
            }
            Err(e) => tracing::warn!("reading Wi-Fi settings failed: {e:#}"),
        }
        if let Some(t) = retry_at {
            sleep_for = sleep_for.min(t.saturating_duration_since(Instant::now()).max(Duration::from_secs(1)));
        }
        tokio::select! {
            _ = state.wifi.wake.notified() => {}
            _ = tokio::time::sleep(sleep_for.max(Duration::from_millis(100))) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_addresses() {
        assert_eq!(normalize_router_url("192.168.1.1").as_deref(), Some("http://192.168.1.1"));
        assert_eq!(normalize_router_url(" http://192.168.1.1/ ").as_deref(), Some("http://192.168.1.1"));
        assert_eq!(normalize_router_url("https://routerlogin.net:8443").as_deref(), Some("https://routerlogin.net:8443"));
        assert_eq!(normalize_router_url("ftp://x"), None);
        assert_eq!(normalize_router_url("http://a/b"), None);
        assert_eq!(normalize_router_url("http://u:p@a"), None);
        assert_eq!(normalize_router_url(""), None);
    }

    #[test]
    fn wanted_follows_the_schedule_only_when_turned_on() {
        let mut row = Row {
            enabled: true,
            router_url: "http://192.168.1.1".into(),
            router_user: "admin".into(),
            router_password: Some("x".into()),
            script_on: None,
            script_off: None,
            script_status: None,
        };
        assert_eq!(row.script(Script::On), script::DEFAULT_ON);
        assert_eq!(wanted(&row, false, Some(true)), Some(false));
        assert_eq!(wanted(&row, true, None), Some(true));
        row.enabled = false;
        // Turned off: undo our own "off", otherwise leave the router alone.
        assert_eq!(wanted(&row, false, Some(false)), Some(true));
        assert_eq!(wanted(&row, false, None), None);
        row.router_password = None;
        row.enabled = true;
        assert_eq!(wanted(&row, false, Some(true)), None);
    }
}
