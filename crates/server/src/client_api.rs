//! Endpoints the client daemons call, authenticated by per-machine bearer tokens.

use crate::auth::Client;
use crate::error::{AppError, AppResult};
use crate::{AppState, db};
use screentime_common::{AccountsReply, AccountsReport, Policy, TimeRequest, UsageReport};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Local;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/client/policy/{username}", get(policy))
        .route("/api/client/usage", post(usage))
        .route("/api/client/requests", post(request_time))
        .route("/api/client/accounts", post(accounts))
}

/// Linux user names: a lowercase letter or underscore, then up to 31 of `[a-z0-9_-]`.
pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z' | '_'))
        && name.len() <= 32
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
        // Taken by the built-in Wi-Fi user.
        && name != crate::wifi::USER
}

fn check_username(name: &str) -> AppResult<()> {
    if valid_username(name) { Ok(()) } else { Err(AppError::BadRequest("invalid username".into())) }
}

async fn policy(State(s): State<AppState>, client: Client, Path(username): Path<String>) -> AppResult<Json<Policy>> {
    check_username(&username)?;
    let user = db::ensure_user(&s.db, &username, s.config.default_allowance_minutes).await?;
    let today = Local::now().date_naive();
    Ok(Json(db::policy_for(&s.db, user, &username, &client.hostname, today).await?))
}

async fn usage(State(s): State<AppState>, client: Client, Json(report): Json<UsageReport>) -> AppResult<StatusCode> {
    check_username(&report.username)?;
    if report.active_minutes > 24 * 60 {
        return Err(AppError::BadRequest("more minutes than a day has".into()));
    }
    let user = db::ensure_user(&s.db, &report.username, s.config.default_allowance_minutes).await?;
    let before: u32 = db::usage(&s.db, user, report.day)
        .await?
        .iter()
        .find(|u| u.host == client.hostname)
        .map_or(0, |u| u.local_bonus_minutes);
    db::record_usage(&s.db, user, &client.hostname, &report).await?;
    // Time approved at the computer itself: tell the parents, in case it wasn't one of them.
    if report.local_bonus_minutes > before {
        let added = report.local_bonus_minutes - before;
        let name = db::display_name(&s.db, user).await?.unwrap_or_else(|| report.username.clone());
        tracing::info!(user = %report.username, host = %client.hostname, added, "time approved at the computer");
        let payload = crate::push::Payload {
            title: format!("{name} got {added} more minutes on {}", client.hostname),
            body: "Approved at the computer with \u{201c}My parent is here\u{201d}. Not you? Change the parent PIN.".into(),
            url: format!("{}/", s.config.public_url),
            tag: None,
            actions: Vec::new(),
        };
        let (notifier, db) = (s.notifier.clone(), s.db.clone());
        tokio::spawn(async move {
            if let Err(e) = notifier.send_push(&db, &payload).await {
                tracing::warn!("push about a computer grant failed: {e:#}");
            }
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn request_time(
    State(s): State<AppState>,
    client: Client,
    Json(req): Json<TimeRequest>,
) -> AppResult<StatusCode> {
    check_username(&req.username)?;
    let user = db::ensure_user(&s.db, &req.username, s.config.default_allowance_minutes).await?;
    let minutes = req.requested_minutes.filter(|m| *m > 0).map(|m| m.min(24 * 60));
    let reason: Option<String> = req.reason.map(|r| r.chars().take(500).collect());
    let (row, token) = db::create_request(&s.db, user, &client.hostname, minutes, reason.as_deref()).await?;
    tracing::info!(id = row.id, user = %row.username, host = %row.host, "time requested");
    let (notifier, db) = (s.notifier.clone(), s.db.clone());
    tokio::spawn(async move { notifier.time_request(&db, &row, &token).await });
    Ok(StatusCode::CREATED)
}

/// The computer reports its local accounts; the reply says which are limited and as which kid.
async fn accounts(
    State(s): State<AppState>,
    client: Client,
    Json(report): Json<AccountsReport>,
) -> AppResult<Json<AccountsReply>> {
    if report.accounts.len() > 500 {
        return Err(AppError::BadRequest("too many accounts".into()));
    }
    for a in &report.accounts {
        if a.name.is_empty() || a.name.len() > 256 {
            return Err(AppError::BadRequest("invalid account name".into()));
        }
        if let Some(kid) = &a.configured_kid {
            check_username(kid)?;
        }
    }
    let limited = db::report_accounts(&s.db, client.id, &report.accounts, s.config.default_allowance_minutes).await?;
    Ok(Json(AccountsReply {
        limited,
        // "My parent is here" takes the same password as the web UI.
        parent_pin_hash: db::admin_password_hash(&s.db).await?.or_else(|| s.config.admin_password_hash.clone()),
    }))
}

#[cfg(test)]
mod tests {
    use super::valid_username;

    #[test]
    fn usernames() {
        assert!(valid_username("kid"));
        assert!(valid_username("_svc-1"));
        assert!(!valid_username(""));
        assert!(!valid_username("Kid"));
        assert!(!valid_username("../etc"));
        assert!(!valid_username("1kid"));
        assert!(!valid_username(&"a".repeat(33)));
    }
}
