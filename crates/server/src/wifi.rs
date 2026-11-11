//! Wi-Fi time: one balance for the whole household. While it's turned on, the balance runs down
//! in real time; at zero the server switches the router's Wi-Fi off, and adding time switches
//! it back on.
//!
//! Nothing touches the router until a parent has saved the router password and turned Wi-Fi
//! time on, so a fresh install (or an upgrade) never cuts the Wi-Fi by itself.

use crate::AppState;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};

/// How often the controller looks at the balance even when nothing woke it.
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
    /// Serializes changes to the balance.
    balance: Mutex<()>,
    router: Mutex<RouterStatus>,
    wake: Notify,
    /// Settings changed: try the router again now instead of waiting out a back-off.
    retry_now: AtomicBool,
}

impl Wifi {
    pub async fn router_status(&self) -> RouterStatus {
        self.router.lock().await.clone()
    }

    fn poke(&self, retry_now: bool) {
        if retry_now {
            self.retry_now.store(true, Ordering::SeqCst);
        }
        self.wake.notify_one();
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Row {
    pub enabled: bool,
    pub remaining_seconds: i64,
    pub counted_at: String,
    pub router_url: String,
    pub router_user: String,
    pub router_password: Option<String>,
}

impl Row {
    /// Seconds left right now.
    pub fn remaining_at(&self, now: DateTime<Utc>) -> i64 {
        if !self.enabled {
            return self.remaining_seconds.max(0);
        }
        let since = DateTime::parse_from_rfc3339(&self.counted_at)
            .map(|t| (now - t.with_timezone(&Utc)).num_seconds().max(0))
            .unwrap_or(0);
        (self.remaining_seconds - since).max(0)
    }

    pub fn password_set(&self) -> bool {
        self.router_password.as_deref().is_some_and(|p| !p.is_empty())
    }
}

pub async fn load(db: &SqlitePool) -> Result<Row> {
    Ok(sqlx::query_as(
        "SELECT enabled, remaining_seconds, counted_at, router_url, router_user, router_password FROM wifi WHERE id = 1",
    )
    .fetch_one(db)
    .await?)
}

/// Write the balance as of now, so a change of rate (enabled or not) starts from the right count.
async fn store_balance(db: &SqlitePool, remaining: i64, now: DateTime<Utc>) -> Result<()> {
    sqlx::query("UPDATE wifi SET remaining_seconds = ?, counted_at = ? WHERE id = 1")
        .bind(remaining.max(0))
        .bind(now.to_rfc3339())
        .execute(db)
        .await?;
    Ok(())
}

/// Add (positive) or take away (negative) minutes. The balance never goes below zero.
/// Returns the seconds left afterwards.
pub async fn adjust(state: &AppState, minutes: i64) -> Result<i64> {
    let _guard = state.wifi.balance.lock().await;
    let now = Utc::now();
    let row = load(&state.db).await?;
    let left = (row.remaining_at(now) + minutes * 60).max(0);
    store_balance(&state.db, left, now).await?;
    drop(_guard);
    state.wifi.poke(false);
    tracing::info!(minutes, left_seconds = left, "Wi-Fi time adjusted");
    Ok(left)
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct SettingsChange {
    pub enabled: Option<bool>,
    pub router_url: Option<String>,
    pub router_user: Option<String>,
    /// A new password; an empty string clears it. Left out keeps the saved one.
    pub router_password: Option<String>,
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
    ok.then(|| with_scheme)
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
    let _guard = state.wifi.balance.lock().await;
    let now = Utc::now();
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
    let left = row.remaining_at(now);
    if let Some(enabled) = change.enabled {
        row.enabled = enabled;
    }
    if row.enabled && !row.password_set() {
        return Err(SettingsError::Invalid("Save the router password first.".into()));
    }
    sqlx::query(
        "UPDATE wifi SET enabled = ?, remaining_seconds = ?, counted_at = ?, router_url = ?, router_user = ?,
         router_password = ? WHERE id = 1",
    )
    .bind(row.enabled)
    .bind(left)
    .bind(now.to_rfc3339())
    .bind(&row.router_url)
    .bind(&row.router_user)
    .bind(&row.router_password)
    .execute(&state.db)
    .await?;
    drop(_guard);
    tracing::info!(enabled = row.enabled, router = %row.router_url, "Wi-Fi settings changed");
    state.wifi.poke(true);
    Ok(())
}

/// Everything the UI shows about Wi-Fi time. Never includes the router password.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    /// Wi-Fi time is being enforced.
    pub enabled: bool,
    pub password_set: bool,
    pub router_url: String,
    pub router_user: String,
    pub remaining_seconds: i64,
    /// Rounded up, so "1 min" shows until it's really gone.
    pub remaining_minutes: i64,
    /// What the router should be set to; `None` when the server leaves it alone.
    pub wanted_on: Option<bool>,
    pub router: RouterStatus,
}

pub async fn overview(state: &AppState) -> Result<Overview> {
    let row = load(&state.db).await?;
    let router = state.wifi.router_status().await;
    let remaining = row.remaining_at(Utc::now());
    Ok(Overview {
        enabled: row.enabled,
        password_set: row.password_set(),
        wanted_on: wanted(&row, remaining, router.on),
        router_url: row.router_url,
        router_user: row.router_user,
        remaining_seconds: remaining,
        remaining_minutes: (remaining + 59) / 60,
        router,
    })
}

/// What the router should be: on while there's time, off at zero. When Wi-Fi time is turned
/// off, the server's last word was "off" gets undone; otherwise it leaves the router alone.
fn wanted(row: &Row, remaining: i64, router_on: Option<bool>) -> Option<bool> {
    if !row.password_set() {
        return None;
    }
    if row.enabled {
        Some(remaining > 0)
    } else if router_on == Some(false) {
        Some(true)
    } else {
        None
    }
}

/// Keep the router in line with the balance. Runs for the life of the server: once at start,
/// then whenever time or settings change, every TICK, and the moment the balance runs out.
pub async fn run(state: AppState) {
    let mut retry_at: Option<Instant> = None;
    loop {
        if state.wifi.retry_now.swap(false, Ordering::SeqCst) {
            retry_at = None;
        }
        let mut sleep_for = TICK;
        match load(&state.db).await {
            Ok(row) => {
                let remaining = row.remaining_at(Utc::now());
                let current = state.wifi.router.lock().await.on;
                if let Some(on) = wanted(&row, remaining, current)
                    && current != Some(on)
                    && retry_at.is_none_or(|t| Instant::now() >= t)
                {
                    let router = netgear::Router {
                        url: &row.router_url,
                        user: &row.router_user,
                        password: row.router_password.as_deref().unwrap_or_default(),
                    };
                    tracing::info!(on, "switching the Wi-Fi");
                    let result = netgear::set_wifi(&router, on).await;
                    let now = Utc::now().to_rfc3339();
                    let mut status = state.wifi.router.lock().await;
                    match result {
                        Ok(()) => {
                            tracing::info!(on, "Wi-Fi switched");
                            status.on = Some(on);
                            status.last_changed_at = Some(now);
                            status.last_error = None;
                            status.last_error_at = None;
                            retry_at = None;
                        }
                        Err(e) => {
                            tracing::warn!(on, "switching the Wi-Fi failed: {}", e.message);
                            status.last_error = Some(e.message);
                            status.last_error_at = Some(now);
                            let wait = if e.bad_password { RETRY_AFTER_BAD_PASSWORD } else { RETRY_AFTER };
                            retry_at = Some(Instant::now() + wait);
                        }
                    }
                    continue;
                }
                // Wake right when the time runs out.
                if row.enabled && remaining > 0 {
                    sleep_for = sleep_for.min(Duration::from_secs(remaining as u64));
                }
            }
            Err(e) => tracing::warn!("reading Wi-Fi time failed: {e:#}"),
        }
        if let Some(t) = retry_at {
            sleep_for = sleep_for.min(t.saturating_duration_since(Instant::now()).max(Duration::from_secs(1)));
        }
        tokio::select! {
            _ = state.wifi.wake.notified() => {}
            _ = tokio::time::sleep(sleep_for) => {}
        }
    }
}

/// The Netgear Nighthawk RS200v2 web admin, driven the same way as the router's own pages:
/// log in, read a one-time token off the Advanced Wireless page, post that page's form with the
/// main 2.4 GHz and 5 GHz radios switched, log out. The 6 GHz radio is left alone.
pub mod netgear {
    use super::*;

    pub struct Router<'a> {
        pub url: &'a str,
        pub user: &'a str,
        pub password: &'a str,
    }

    #[derive(Debug)]
    pub struct Failure {
        pub message: String,
        /// The router refused the login; trying again soon would lock the admin out.
        pub bad_password: bool,
    }

    impl Failure {
        fn new(message: impl Into<String>) -> Failure {
            Failure { message: message.into(), bad_password: false }
        }
    }

    /// Extra attempts after the first one, and the pause between them.
    const RETRIES: u32 = 2;
    const RETRY_WAIT: Duration = Duration::from_secs(5);

    pub async fn set_wifi(router: &Router<'_>, on: bool) -> Result<(), Failure> {
        let mut last = Failure::new("not tried");
        for i in 0..=RETRIES {
            if i > 0 {
                tokio::time::sleep(RETRY_WAIT).await;
            }
            match attempt(router, on).await {
                Ok(()) => return Ok(()),
                Err(e) if e.bad_password => return Err(e),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// The router's own login page sends the password like this.
    pub fn encode_twice(password: &str) -> String {
        use base64ct::{Base64, Encoding};
        const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ=";
        let r = || CHARS[rand::random_range(0..CHARS.len())] as char;
        let inner = Base64::encode_string(password.as_bytes());
        Base64::encode_string(format!("{}{inner}{}", r(), r()).as_bytes())
    }

    /// The one-time token in the page's form action: `/apply.cgi?/WLG_adv.htm timestamp=123`.
    pub fn form_token(html: &str) -> Option<&str> {
        const MARK: &str = "/apply.cgi?/WLG_adv.htm";
        let mut rest = html;
        while let Some(i) = rest.find(MARK) {
            rest = &rest[i + MARK.len()..];
            if let Some(after) = rest.strip_prefix(' ').or_else(|| rest.strip_prefix("%20"))
                && let Some(after) = after.strip_prefix("timestamp=")
            {
                let n = after.bytes().take_while(u8::is_ascii_digit).count();
                if n > 0 {
                    return Some(&after[..n]);
                }
            }
        }
        None
    }

    /// Why the settings page came back without its token.
    pub fn explain_missing_token(html: &str, login_status: u16, page_status: u16) -> Failure {
        let l = html.to_ascii_lowercase();
        let multi_login = l.match_indices("multi").any(|(i, _)| {
            let rest = &l[i + 5..];
            rest.starts_with("login") || rest.get(1..).is_some_and(|r| r.starts_with("login"))
        });
        if multi_login || l.contains("already logged in") || l.contains("another user") || l.contains("another admin") {
            return Failure::new("another admin session is open on the router (its web page or the Nighthawk app)");
        }
        if l.contains("admin_login") || ["type=password", "type=\"password", "type='password"].iter().any(|p| l.contains(p)) {
            return Failure {
                message: "the router refused the login (wrong password, or locked out after too many tries)".into(),
                bad_password: true,
            };
        }
        Failure::new(format!(
            "the router didn't return its settings page (login HTTP {login_status}, page HTTP {page_status})"
        ))
    }

    /// The Advanced Wireless form as captured from the router. Only the two radio switches change
    /// (plus `enable_ap`/`enable_ap_an` when turning on); everything else goes back unchanged.
    pub const FORM: &[(&str, &str)] = &[
        ("submit_flag", "wlan_adv"),
        ("wl_rts", "2347"),
        ("wl_frag", "2346"),
        ("wl_enable_shortpreamble", "0"),
        ("wl_tx_ctrl", "100"),
        ("wl_enable_router", "0"),
        ("endis_pin", "0"),
        ("hid_protect_enable", "1"),
        ("hid_super_wifi", "0"),
        ("hid_super_wifi_an", "0"),
        ("wla_rts", "2347"),
        ("wla_frag", ""),
        ("wla_enable_shortpreamble", "0"),
        ("wla_tx_ctrl", "100"),
        ("wla_enable_router", "0"),
        ("hid_super_wifi_tri", "0"),
        ("wla_2nd_rts", ""),
        ("wla_2nd_frag", ""),
        ("wla_2nd_enable_shortpreamble", ""),
        ("wla_2nd_tx_ctrl", ""),
        ("wla_2nd_enable_router", "0"),
        ("wla_6g_rts", "2347"),
        ("wla_6g_frag", ""),
        ("wla_6g_enable_shortpreamble", "0"),
        ("wla_6g_enable_router", "1"),
        ("wig_enable_router", "ignore"),
        ("wds_change_ip", "still_lanip"),
        ("enable_router", "ignore"),
        ("enable_ssid_broadcast", "ignore"),
        ("endis_wsc_config", "5"),
        ("endis_wsc_config_a", "5"),
        ("endis_wsc_config_tri", "1"),
        ("wladv_endis_wmm", "1"),
        ("wladv_endis_wmm_a", "1"),
        ("wladv_endis_wmm_tri", ""),
        ("wladv_endis_wmm_6g", "0"),
        ("hid_dyn_get_ip", ""),
        ("hid_ap_ipaddr", ""),
        ("hid_ap_subnet", ""),
        ("hid_ap_gateway", ""),
        ("ap_dnsaddr1", ""),
        ("ap_dnsaddr2", ""),
        ("wladv_enable_schedule", "0"),
        ("wladv_enable_schedule_a", "0"),
        ("wladv_enable_schedule_tri", "0"),
        ("wladv_enable_schedule_6g", "0"),
        ("wladv_schedule_type", ""),
        ("wladv_schedule_edit_num", ""),
        ("wladv_schedule_delete_num", ""),
        ("hid_enable_coexist", "0"),
        ("hid_wlg_mu_mimo", "1"),
        ("hid_wla_beamforming", "1"),
        ("hid_wla_mu_mimo", "1"),
        ("hid_wla_bf", "1"),
        ("hid_enable_atf", "1"),
        ("hid_wla_ht160", "1"),
        ("hid_11k", "0"),
        ("hid_disable_pmf", "0"),
        ("hid_wla_2nd_beamforming", ""),
        ("hid_wla_2nd_mu_mimo", "1"),
        ("hid_wla_2nd_bf", ""),
        ("hid_wla_2nd_ht160", "1"),
        ("hid_wla_6g_mu_mimo", "1"),
        ("hid_wla_6g_beamforming", ""),
        ("hid_wla_6g_bf", "1"),
        ("hid_wla_6g_ofdma", "1"),
        ("hid_mlo_mode", "0"),
        ("hid_mlo_change_reboot", ""),
        ("enable_coexistence", "on"),
        ("wmm_enable", "1"),
        ("frag", "2346"),
        ("rts", "2347"),
        ("enable_shortpreamble", "automatic"),
        ("tx_power_ctrl", "100"),
        ("wmm_enable_a", "1"),
        ("frag_an", "2346"),
        ("rts_an", "2347"),
        ("enable_shortpreamble_an", "automatic"),
        ("tx_power_ctrl_an", "100"),
        ("enable_ap_6g", "1"),
        ("frag_6g", "2346"),
        ("rts_6g", "2347"),
        ("enable_short_preamble_6g", "automatic"),
        ("tx_power_ctrl_6g", "100"),
        ("pin_disable", "1"),
        ("prevent_pin_compromise", "on"),
        ("pin_attack_count", "3"),
        ("wsc_config", "on"),
        ("wla_wsc_config", "on"),
        ("enable_mu", ""),
        ("enable_atf", "on"),
        ("enable_ht160", ""),
    ];

    pub fn form(on: bool) -> Vec<(&'static str, &'static str)> {
        let enable = if on { "1" } else { "0" };
        let mut body: Vec<_> = FORM
            .iter()
            .map(|&(k, v)| match k {
                "wl_enable_router" | "wla_enable_router" => (k, enable),
                _ => (k, v),
            })
            .collect();
        if on {
            body.push(("enable_ap", "1"));
            body.push(("enable_ap_an", "1"));
        }
        body
    }

    fn unreachable(router: &Router<'_>, e: reqwest::Error) -> Failure {
        if e.is_timeout() {
            return Failure::new(format!("the router at {} didn't answer in time", router.url));
        }
        // reqwest's own message is just "error sending request"; the cause is further down.
        let mut cause = String::new();
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            cause = s.to_string();
            source = s.source();
        }
        if cause.is_empty() {
            cause = e.without_url().to_string();
        }
        Failure::new(format!("couldn't reach the router at {} ({cause})", router.url))
    }

    /// One login → token → change → logout cycle.
    async fn attempt(router: &Router<'_>, on: bool) -> Result<(), Failure> {
        let base = router.url.trim_end_matches('/');
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .map_err(|e| Failure::new(format!("HTTP client: {e}")))?;

        let login = http
            .post(format!("{base}/apply.cgi?%20timestamp=undefined"))
            .timeout(Duration::from_secs(10))
            .form(&[
                ("submit_flag", "admin_login"),
                ("username", router.user),
                ("password", &encode_twice(router.password)),
            ])
            .send()
            .await
            .map_err(|e| unreachable(router, e))?;
        let login_status = login.status().as_u16();
        let _ = login.bytes().await;

        let result = change(&http, router, base, on, login_status).await;
        // The router allows one admin session at a time, so always end ours.
        let _ = http.get(format!("{base}/LGO_logout.htm")).timeout(Duration::from_secs(5)).send().await;
        result
    }

    async fn change(http: &reqwest::Client, router: &Router<'_>, base: &str, on: bool, login_status: u16) -> Result<(), Failure> {
        let page = http
            .get(format!("{base}/WLG_adv.htm"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| unreachable(router, e))?;
        let page_status = page.status().as_u16();
        let html = page.text().await.map_err(|e| unreachable(router, e))?;
        // Without the token the router answers 400, so don't send the change.
        let Some(token) = form_token(&html) else {
            return Err(explain_missing_token(&html, login_status, page_status));
        };
        let applied = http
            .post(format!("{base}/apply.cgi?/WLG_adv.htm%20timestamp={token}"))
            .timeout(Duration::from_secs(20))
            .header(reqwest::header::REFERER, format!("{base}/WLG_adv.htm"))
            .form(&form(on))
            .send()
            .await
            .map_err(|e| unreachable(router, e))?;
        let code = applied.status().as_u16();
        let _ = applied.bytes().await;
        if (200..400).contains(&code) {
            Ok(())
        } else {
            Err(Failure::new(format!("the router rejected the change (HTTP {code})")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::netgear::*;
    use super::*;

    #[test]
    fn finds_the_form_token() {
        let html = r#"<form action="/apply.cgi?/WLG_adv.htm timestamp=123456" method="post">"#;
        assert_eq!(form_token(html), Some("123456"));
        assert_eq!(form_token("x /apply.cgi?/WLG_adv.htm%20timestamp=42\""), Some("42"));
        assert_eq!(form_token("/apply.cgi?/WLG_adv.htm timestamp=\" /apply.cgi?/WLG_adv.htm timestamp=7"), Some("7"));
        assert_eq!(form_token("<input type=password name=password>"), None);
    }

    #[test]
    fn password_encoding_round_trips() {
        use base64ct::{Base64, Encoding};
        let outer = Base64::decode_vec(&encode_twice("hunter2")).unwrap();
        let outer = String::from_utf8(outer).unwrap();
        let inner = &outer[1..outer.len() - 1];
        assert_eq!(Base64::decode_vec(inner).unwrap(), b"hunter2");
    }

    #[test]
    fn explains_why_there_was_no_token() {
        assert!(explain_missing_token("<input type=\"password\">", 200, 200).bad_password);
        let multi = explain_missing_token("multi_login.html", 200, 200);
        assert!(!multi.bad_password && multi.message.contains("another admin"));
        assert!(explain_missing_token("", 500, 404).message.contains("HTTP 500"));
    }

    #[test]
    fn form_switches_only_the_main_radios() {
        let off = form(false);
        let on = form(true);
        let get = |f: &[(&str, &str)], k: &str| f.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert_eq!(get(&off, "wl_enable_router").as_deref(), Some("0"));
        assert_eq!(get(&on, "wla_enable_router").as_deref(), Some("1"));
        assert_eq!(get(&on, "wla_6g_enable_router").as_deref(), Some("1"));
        assert_eq!(get(&off, "wla_6g_enable_router").as_deref(), Some("1"));
        assert_eq!(get(&on, "enable_ap").as_deref(), Some("1"));
        assert_eq!(get(&off, "enable_ap"), None);
    }

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
    fn balance_runs_down_only_while_enabled() {
        let t0: DateTime<Utc> = "2026-11-11T10:00:00Z".parse().unwrap();
        let mut row = Row {
            enabled: true,
            remaining_seconds: 600,
            counted_at: t0.to_rfc3339(),
            router_url: "http://192.168.1.1".into(),
            router_user: "admin".into(),
            router_password: Some("x".into()),
        };
        assert_eq!(row.remaining_at(t0 + chrono::Duration::seconds(90)), 510);
        assert_eq!(row.remaining_at(t0 + chrono::Duration::hours(2)), 0);
        assert_eq!(wanted(&row, 0, Some(true)), Some(false));
        assert_eq!(wanted(&row, 5, None), Some(true));
        row.enabled = false;
        assert_eq!(row.remaining_at(t0 + chrono::Duration::hours(2)), 600);
        // Turned off: undo our own "off", otherwise leave the router alone.
        assert_eq!(wanted(&row, 600, Some(false)), Some(true));
        assert_eq!(wanted(&row, 600, None), None);
        row.router_password = None;
        row.enabled = true;
        assert_eq!(wanted(&row, 0, Some(true)), None);
    }
}
