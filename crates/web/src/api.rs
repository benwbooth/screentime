//! JSON types and calls for the server's admin API.

use screentime_common::{Decision, DowntimeWindow};
use chrono::NaiveDate;
use gloo_net::http::{Request, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub type Allowances = [Option<u32>; 7];

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HostUsage {
    pub host: String,
    pub active_minutes: u32,
    pub local_bonus_minutes: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UserOverview {
    pub username: String,
    /// The built-in Wi-Fi user rather than a kid.
    #[serde(default)]
    pub wifi: bool,
    #[serde(default)]
    pub time_left: Option<u32>,
    pub name: String,
    pub allowances: Allowances,
    pub downtime: Vec<DowntimeWindow>,
    pub bonus_today: i64,
    #[serde(default)]
    pub locks: Vec<String>,
    pub usage_today: Vec<HostUsage>,
    pub used_today: u32,
    pub decision: Decision,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RequestRow {
    pub id: i64,
    pub username: String,
    pub host: String,
    pub requested_minutes: Option<i64>,
    pub reason: Option<String>,
    pub status: String,
    pub granted_minutes: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccountRow {
    pub name: String,
    pub admin: bool,
    pub kid: Option<String>,
    pub new: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClientRow {
    pub hostname: String,
    pub last_seen: Option<String>,
    #[serde(default)]
    pub accounts: Vec<AccountRow>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Overview {
    pub today: NaiveDate,
    pub users: Vec<UserOverview>,
    pub pending_requests: Vec<RequestRow>,
    pub clients: Vec<ClientRow>,
    #[serde(default)]
    pub wifi: Option<Wifi>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RouterStatus {
    pub on: Option<bool>,
    pub last_changed_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
}

/// The router side of Wi-Fi time; its schedule, time and history belong to the built-in Wi-Fi
/// user. The router password itself never comes back, only whether it's set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Wifi {
    pub username: String,
    pub enabled: bool,
    pub password_set: bool,
    pub router_url: String,
    pub router_user: String,
    pub wanted_on: Option<bool>,
    pub router: RouterStatus,
    /// "on", "off" and/or "status": scripts changed from the built-in ones.
    #[serde(default)]
    pub custom_scripts: Vec<String>,
}

/// The router scripts as saved, with the built-in ones for "Reset to default".
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Scripts {
    pub on: String,
    pub off: String,
    pub status: String,
    pub default_on: String,
    pub default_off: String,
    pub default_status: String,
}

/// What a script run did.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ScriptOutcome {
    pub ok: bool,
    #[serde(default)]
    pub value: serde_json::Value,
    pub message: Option<String>,
    #[serde(default)]
    pub logs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PushInfo {
    pub public_key: String,
    pub subscriptions: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewClientToken {
    pub hostname: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// Not signed in, or the session expired.
    Unauthorized,
    Message(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Please sign in again."),
            ApiError::Message(m) => write!(f, "{m}"),
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

thread_local! {
    /// The UI version the server named in its first reply since this page loaded.
    static UI_VERSION: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The server was upgraded while this page was open: load the new UI rather than misread the
/// new API with the old one. Returns true when a reload is under way.
fn reload_if_upgraded(resp: &Response) -> bool {
    let Some(version) = resp.headers().get("x-screentime-ui") else { return false };
    let upgraded = UI_VERSION.with(|v| {
        let mut v = v.borrow_mut();
        match v.as_deref() {
            None => {
                *v = Some(version);
                false
            }
            Some(old) => old != version,
        }
    });
    if upgraded && let Some(w) = web_sys::window() {
        let _ = w.location().reload();
    }
    upgraded
}

async fn check(resp: Result<Response, gloo_net::Error>) -> ApiResult<Response> {
    let resp = resp.map_err(|e| ApiError::Message(format!("Can't reach the server ({e}).")))?;
    if reload_if_upgraded(&resp) {
        return Err(ApiError::Message("Screen time was updated; reloading\u{2026}".into()));
    }
    match resp.status() {
        200..=299 => Ok(resp),
        401 => Err(ApiError::Unauthorized),
        429 => Err(ApiError::Message("Too many wrong passwords. Try again in a few minutes.".into())),
        _ => {
            let text = resp.text().await.unwrap_or_default();
            Err(ApiError::Message(if text.is_empty() { format!("Error {}", resp.status()) } else { text }))
        }
    }
}

async fn json<T: DeserializeOwned>(resp: Response) -> ApiResult<T> {
    resp.json().await.map_err(|e| ApiError::Message(format!("Unexpected reply ({e}).")))
}

async fn get<T: DeserializeOwned>(url: &str) -> ApiResult<T> {
    json(check(Request::get(url).send().await).await?).await
}

async fn send<B: Serialize>(method: &str, url: &str, body: &B) -> ApiResult<Response> {
    let builder = match method {
        "PUT" => Request::put(url),
        "DELETE" => Request::delete(url),
        _ => Request::post(url),
    };
    let req = builder.json(body).map_err(|e| ApiError::Message(e.to_string()))?;
    check(req.send().await).await
}

pub async fn login(password: &str) -> ApiResult<()> {
    send("POST", "/api/login", &serde_json::json!({ "password": password })).await.map(drop)
}

pub async fn logout() -> ApiResult<()> {
    send("POST", "/api/logout", &serde_json::json!({})).await.map(drop)
}

pub async fn change_password(current: &str, new: &str) -> ApiResult<()> {
    send("POST", "/api/password", &serde_json::json!({ "current": current, "new": new })).await.map(drop)
}

/// One day of a kid's use: the total and minutes in each hour, midnight first.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DayHistory {
    pub day: chrono::NaiveDate,
    pub minutes: u32,
    pub hours: Vec<u32>,
    #[serde(default)]
    pub sessions: Vec<PlaySession>,
}

/// A stretch of continuous use on one computer; times are RFC 3339 in UTC.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PlaySession {
    pub host: String,
    pub started_at: String,
    pub ended_at: String,
    pub minutes: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KidHistory {
    pub username: String,
    #[serde(default)]
    pub wifi: bool,
    pub name: String,
    pub days: Vec<DayHistory>,
}

pub async fn history(days: u32) -> ApiResult<Vec<KidHistory>> {
    get(&format!("/api/history?days={days}")).await
}

pub async fn overview() -> ApiResult<Overview> {
    get("/api/overview").await
}

/// The page's own periodic refresh, which doesn't count as the parent using it.
pub async fn overview_in_background() -> ApiResult<Overview> {
    json(check(Request::get("/api/overview").header("X-Screentime-Background", "1").send().await).await?).await
}

/// Tell the server the parent is still here, so it doesn't sign them out.
pub async fn keepalive() -> ApiResult<()> {
    send("POST", "/api/keepalive", &serde_json::json!({})).await.map(drop)
}

pub async fn add_user(username: &str) -> ApiResult<()> {
    send("POST", "/api/users", &serde_json::json!({ "username": username })).await.map(drop)
}

/// Positive adds time for today, negative takes it away.
pub async fn add_bonus(username: &str, minutes: i64) -> ApiResult<()> {
    send("POST", &format!("/api/users/{username}/bonus"), &serde_json::json!({ "minutes": minutes })).await.map(drop)
}

pub async fn set_allowances(username: &str, allowances: &Allowances) -> ApiResult<()> {
    send("PUT", &format!("/api/users/{username}/allowances"), allowances).await.map(drop)
}

pub async fn set_downtime(username: &str, windows: &[DowntimeWindow]) -> ApiResult<()> {
    send("PUT", &format!("/api/users/{username}/downtime"), &windows).await.map(drop)
}

pub async fn resolve_request(id: i64, minutes: u32) -> ApiResult<()> {
    send("POST", &format!("/api/requests/{id}/resolve"), &serde_json::json!({ "minutes": minutes })).await.map(drop)
}

pub async fn add_client(hostname: &str) -> ApiResult<NewClientToken> {
    json(send("POST", "/api/clients", &serde_json::json!({ "hostname": hostname })).await?).await
}

pub async fn set_account(hostname: &str, account: &str, kid: Option<&str>) -> ApiResult<()> {
    send("PUT", &format!("/api/clients/{hostname}/accounts/{account}"), &serde_json::json!({ "kid": kid })).await.map(drop)
}

/// Lock or unlock a kid on one computer, or on all of them with `None`.
pub async fn set_lock(username: &str, computer: Option<&str>, locked: bool) -> ApiResult<()> {
    send("PUT", &format!("/api/users/{username}/lock"), &serde_json::json!({ "computer": computer, "locked": locked }))
        .await
        .map(drop)
}

pub async fn set_name(username: &str, name: &str) -> ApiResult<()> {
    send("PUT", &format!("/api/users/{username}/name"), &serde_json::json!({ "name": name })).await.map(drop)
}

pub async fn remove_client(hostname: &str) -> ApiResult<()> {
    send("DELETE", &format!("/api/clients/{hostname}"), &serde_json::json!({})).await.map(drop)
}

pub async fn push_info() -> ApiResult<PushInfo> {
    get("/api/push").await
}

pub async fn push_subscribe(subscription_json: &str, label: &str) -> ApiResult<()> {
    let mut sub: serde_json::Value =
        serde_json::from_str(subscription_json).map_err(|e| ApiError::Message(e.to_string()))?;
    sub["label"] = label.into();
    send("POST", "/api/push/subscribe", &sub).await.map(drop)
}

pub async fn push_test() -> ApiResult<usize> {
    #[derive(Deserialize)]
    struct Delivered {
        delivered: usize,
    }
    let d: Delivered = json(send("POST", "/api/push/test", &serde_json::json!({})).await?).await?;
    Ok(d.delivered)
}

/// Change Wi-Fi settings; fields left `None` keep their saved values.
pub async fn set_wifi(
    enabled: Option<bool>,
    router_url: Option<&str>,
    router_user: Option<&str>,
    router_password: Option<&str>,
) -> ApiResult<()> {
    let body = serde_json::json!({
        "enabled": enabled,
        "router_url": router_url,
        "router_user": router_user,
        "router_password": router_password,
    });
    send("PUT", "/api/wifi/settings", &body).await.map(drop)
}

pub async fn wifi_scripts() -> ApiResult<Scripts> {
    get("/api/wifi/scripts").await
}

/// Save one router script ("on", "off" or "status"); an empty one goes back to the built-in.
pub async fn save_script(which: &str, code: &str) -> ApiResult<()> {
    let body = serde_json::json!({ format!("script_{which}"): code });
    send("PUT", "/api/wifi/settings", &body).await.map(drop)
}

/// Run a router script now, as edited.
pub async fn test_script(which: &str, code: &str) -> ApiResult<ScriptOutcome> {
    json(send("POST", "/api/wifi/test", &serde_json::json!({ "script": which, "code": code })).await?).await
}
