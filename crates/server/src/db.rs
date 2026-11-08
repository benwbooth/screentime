//! Queries. Everything takes the pool so handlers stay thin.

use crate::auth::{hash_token, new_token};
use screentime_common::{DowntimeWindow, Policy, UsageReport};
use anyhow::Result;
use chrono::{Datelike, NaiveDate, Timelike, Utc, Weekday};
use serde::Serialize;
use sqlx::SqlitePool;

pub type Allowances = [Option<u32>; 7];

fn days_to_mask(days: &[Weekday]) -> i64 {
    days.iter().fold(0, |m, d| m | 1 << d.num_days_from_monday())
}

fn mask_to_days(mask: i64) -> Vec<Weekday> {
    (0..7u8)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| Weekday::try_from(i).ok())
        .collect()
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

pub async fn user_id(db: &SqlitePool, username: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(db)
        .await?)
}

/// The user's id, creating the user with `default_minutes` every day if they're new.
pub async fn ensure_user(db: &SqlitePool, username: &str, default_minutes: u32) -> Result<i64> {
    if let Some(id) = user_id(db, username).await? {
        return Ok(id);
    }
    let mut tx = db.begin().await?;
    let id: i64 = sqlx::query_scalar("INSERT INTO users (username) VALUES (?) RETURNING id")
        .bind(username)
        .fetch_one(&mut *tx)
        .await?;
    for weekday in 0..7 {
        sqlx::query("INSERT INTO daily_allowances (user_id, weekday, minutes) VALUES (?, ?, ?)")
            .bind(id)
            .bind(weekday)
            .bind(default_minutes)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    tracing::info!(username, "added new managed user");
    Ok(id)
}

pub async fn usernames(db: &SqlitePool) -> Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as("SELECT id, username FROM users ORDER BY username").fetch_all(db).await?)
}

pub async fn allowances(db: &SqlitePool, user: i64) -> Result<Allowances> {
    let rows: Vec<(i64, i64)> =
        sqlx::query_as("SELECT weekday, minutes FROM daily_allowances WHERE user_id = ?")
            .bind(user)
            .fetch_all(db)
            .await?;
    let mut out = [None; 7];
    for (weekday, minutes) in rows {
        out[weekday as usize] = Some(minutes as u32);
    }
    Ok(out)
}

pub async fn set_allowances(db: &SqlitePool, user: i64, allowances: &Allowances) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM daily_allowances WHERE user_id = ?").bind(user).execute(&mut *tx).await?;
    for (weekday, minutes) in allowances.iter().enumerate() {
        if let Some(minutes) = minutes {
            sqlx::query("INSERT INTO daily_allowances (user_id, weekday, minutes) VALUES (?, ?, ?)")
                .bind(user)
                .bind(weekday as i64)
                .bind(minutes)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

pub async fn downtime(db: &SqlitePool, user: i64) -> Result<Vec<DowntimeWindow>> {
    let rows: Vec<(i64, chrono::NaiveTime, chrono::NaiveTime)> = sqlx::query_as(
        "SELECT days, start_time, end_time FROM downtime_windows WHERE user_id = ? ORDER BY id",
    )
    .bind(user)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(days, start, end)| DowntimeWindow { days: mask_to_days(days), start, end })
        .collect())
}

pub async fn set_downtime(db: &SqlitePool, user: i64, windows: &[DowntimeWindow]) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM downtime_windows WHERE user_id = ?").bind(user).execute(&mut *tx).await?;
    for w in windows {
        sqlx::query("INSERT INTO downtime_windows (user_id, days, start_time, end_time) VALUES (?, ?, ?, ?)")
            .bind(user)
            .bind(days_to_mask(&w.days))
            .bind(w.start)
            .bind(w.end)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The day's time adjustments: time added, minus time taken away. Can be negative.
pub async fn bonus(db: &SqlitePool, user: i64, day: NaiveDate) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COALESCE(SUM(minutes), 0) FROM bonus_grants WHERE user_id = ? AND day = ?")
        .bind(user)
        .bind(day)
        .fetch_one(db)
        .await?)
}

/// The kid's time left today across every computer, before downtime: the day's limit plus time
/// added anywhere, minus time used. Negative only if more was taken than was left; `None` means
/// no daily limit.
pub async fn time_left(db: &SqlitePool, user: i64, day: NaiveDate) -> Result<Option<i64>> {
    let Some(limit) = allowances(db, user).await?[day.weekday().num_days_from_monday() as usize] else {
        return Ok(None);
    };
    let hosts = usage(db, user, day).await?;
    let used: i64 = hosts.iter().map(|u| u.active_minutes as i64).sum();
    let at_computers: i64 = hosts.iter().map(|u| u.local_bonus_minutes as i64).sum();
    Ok(Some(limit as i64 + bonus(db, user, day).await? + at_computers - used))
}

/// Add (positive) or take away (negative) time for the day.
pub async fn add_bonus(db: &SqlitePool, user: i64, day: NaiveDate, minutes: i64, source: &str) -> Result<()> {
    sqlx::query("INSERT INTO bonus_grants (user_id, day, minutes, source, granted_at) VALUES (?, ?, ?, ?, ?)")
        .bind(user)
        .bind(day)
        .bind(minutes)
        .bind(source)
        .bind(now())
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HostUsage {
    pub host: String,
    pub active_minutes: u32,
    pub local_bonus_minutes: u32,
}

pub async fn usage(db: &SqlitePool, user: i64, day: NaiveDate) -> Result<Vec<HostUsage>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT host, active_minutes, local_bonus_minutes FROM usage WHERE user_id = ? AND day = ? ORDER BY host",
    )
    .bind(user)
    .bind(day)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(host, a, l)| HostUsage { host, active_minutes: a as u32, local_bonus_minutes: l as u32 })
        .collect())
}

pub async fn record_usage(db: &SqlitePool, user: i64, host: &str, report: &UsageReport) -> Result<()> {
    let mut tx = db.begin().await?;
    let before: u32 = sqlx::query_scalar::<_, i64>("SELECT active_minutes FROM usage WHERE user_id = ? AND host = ? AND day = ?")
        .bind(user)
        .bind(host)
        .bind(report.day)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0) as u32;
    // Reports come every few seconds, so the minutes added since the last one belong to the
    // hour it is now. A late report for an earlier day only updates that day's total.
    let now_local = chrono::Local::now();
    let added = report.active_minutes.saturating_sub(before);
    if added > 0 && report.day == now_local.date_naive() {
        sqlx::query(
            "INSERT INTO usage_hours (user_id, host, day, hour, minutes) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (user_id, host, day, hour) DO UPDATE SET minutes = minutes + excluded.minutes",
        )
        .bind(user)
        .bind(host)
        .bind(report.day)
        .bind(now_local.hour() as i64)
        .bind(added.min(60))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO usage (user_id, host, day, active_minutes, local_bonus_minutes, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (user_id, host, day) DO UPDATE SET
           active_minutes = MAX(active_minutes, excluded.active_minutes),
           local_bonus_minutes = MAX(local_bonus_minutes, excluded.local_bonus_minutes),
           updated_at = excluded.updated_at",
    )
    .bind(user)
    .bind(host)
    .bind(report.day)
    .bind(report.active_minutes)
    .bind(report.local_bonus_minutes)
    .bind(now())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// One day of a kid's use across all computers: the total and how it spread over the hours.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DayHistory {
    pub day: NaiveDate,
    pub minutes: u32,
    /// Minutes in each hour, midnight first; all zero for days before hours were recorded.
    pub hours: [u32; 24],
}

/// The kid's last `days` days, oldest first, including days with no use.
pub async fn history(db: &SqlitePool, user: i64, today: NaiveDate, days: u32) -> Result<Vec<DayHistory>> {
    let first = today - chrono::Duration::days(days as i64 - 1);
    let mut out: Vec<DayHistory> = (0..days)
        .map(|i| DayHistory { day: first + chrono::Duration::days(i as i64), minutes: 0, hours: [0; 24] })
        .collect();
    let totals: Vec<(NaiveDate, i64)> =
        sqlx::query_as("SELECT day, SUM(active_minutes) FROM usage WHERE user_id = ? AND day >= ? AND day <= ? GROUP BY day")
            .bind(user)
            .bind(first)
            .bind(today)
            .fetch_all(db)
            .await?;
    for (day, minutes) in totals {
        if let Some(d) = out.get_mut((day - first).num_days() as usize) {
            d.minutes = minutes as u32;
        }
    }
    let hours: Vec<(NaiveDate, i64, i64)> = sqlx::query_as(
        "SELECT day, hour, SUM(minutes) FROM usage_hours WHERE user_id = ? AND day >= ? AND day <= ? GROUP BY day, hour",
    )
    .bind(user)
    .bind(first)
    .bind(today)
    .fetch_all(db)
    .await?;
    for (day, hour, minutes) in hours {
        if let (Some(d), Ok(h)) = (out.get_mut((day - first).num_days() as usize), usize::try_from(hour)) {
            if h < 24 {
                d.hours[h] = minutes as u32;
            }
        }
    }
    Ok(out)
}

/// The policy a client on `host` should enforce today. A kid's time is shared across machines,
/// so time used on other machines is taken off the day's allowance first, then off its bonus.
pub async fn policy_for(db: &SqlitePool, user: i64, username: &str, host: &str, today: NaiveDate) -> Result<Policy> {
    let allowance = allowances(db, user).await?[today.weekday().num_days_from_monday() as usize];
    let elsewhere: Vec<HostUsage> = usage(db, user, today).await?.into_iter().filter(|u| u.host != host).collect();
    let used_elsewhere: u32 = elsewhere.iter().map(|u| u.active_minutes).sum();
    // Time a parent added at another computer counts everywhere, like time added here; the
    // computer it was added at counts its own.
    let local_elsewhere: i64 = elsewhere.iter().map(|u| u.local_bonus_minutes as i64).sum();
    let bonus = bonus(db, user, today).await? + local_elsewhere;
    let (allowance, bonus, taken) = match allowance {
        Some(a) => {
            // What's left before this computer's own use and its own approvals, which it adds
            // itself. Below zero, the rest comes off time approved at this computer.
            let left = a as i64 + bonus - used_elsewhere as i64;
            let a_left = (a as i64 - used_elsewhere as i64).clamp(0, left.max(0));
            (Some(a_left as u32), (left.max(0) - a_left) as u32, (-left).max(0) as u32)
        }
        // No daily limit: adjustments don't matter.
        None => (None, bonus.max(0) as u32, 0),
    };
    Ok(Policy {
        username: username.to_owned(),
        day: today,
        daily_allowance_minutes: allowance,
        bonus_minutes: bonus,
        downtime: downtime(db, user).await?,
        locked: is_locked(db, user, host).await?,
        taken_minutes: taken,
    })
}

/// Whether the kid is locked out of `host` (an empty host asks about the all-computers lock only).
pub async fn is_locked(db: &SqlitePool, user: i64, host: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM locks WHERE user_id = ? AND (host = '*' OR host = ?)")
        .bind(user)
        .bind(host)
        .fetch_one(db)
        .await?;
    Ok(n > 0)
}

/// The kid's locks: '*' for all computers, otherwise hostnames.
pub async fn locks(db: &SqlitePool, user: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT host FROM locks WHERE user_id = ? ORDER BY host").bind(user).fetch_all(db).await?)
}

/// Lock or unlock a kid on one computer, or on all of them with host '*'. Unlocking all also
/// clears the per-computer locks.
pub async fn set_lock(db: &SqlitePool, user: i64, host: &str, locked: bool) -> Result<()> {
    if locked {
        sqlx::query("INSERT OR IGNORE INTO locks (user_id, host, created_at) VALUES (?, ?, ?)")
            .bind(user)
            .bind(host)
            .bind(now())
            .execute(db)
            .await?;
    } else if host == "*" {
        sqlx::query("DELETE FROM locks WHERE user_id = ?").bind(user).execute(db).await?;
    } else {
        sqlx::query("DELETE FROM locks WHERE user_id = ? AND host = ?").bind(user).bind(host).execute(db).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, sqlx::FromRow)]
pub struct RequestRow {
    pub id: i64,
    pub username: String,
    pub host: String,
    pub requested_minutes: Option<i64>,
    pub reason: Option<String>,
    pub status: String,
    pub granted_minutes: Option<i64>,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

const REQUEST_COLUMNS: &str = "r.id, u.username, r.host, r.requested_minutes, r.reason, r.status,
    r.granted_minutes, r.created_at, r.resolved_at
    FROM time_requests r JOIN users u ON u.id = r.user_id";

/// Store a new pending request and return it with the token for its notification links.
pub async fn create_request(
    db: &SqlitePool,
    user: i64,
    host: &str,
    minutes: Option<u32>,
    reason: Option<&str>,
) -> Result<(RequestRow, String)> {
    let token = new_token();
    // One open request per kid: a new one replaces any still unanswered.
    sqlx::query("UPDATE time_requests SET status = 'replaced', resolved_at = ? WHERE user_id = ? AND status = 'pending'")
        .bind(now())
        .bind(user)
        .execute(db)
        .await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO time_requests (user_id, host, requested_minutes, reason, action_token_hash, created_at)
         VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(user)
    .bind(host)
    .bind(minutes)
    .bind(reason)
    .bind(hash_token(&token))
    .bind(now())
    .fetch_one(db)
    .await?;
    let row = get_request(db, id).await?.expect("just inserted");
    Ok((row, token))
}

pub async fn get_request(db: &SqlitePool, id: i64) -> Result<Option<RequestRow>> {
    Ok(sqlx::query_as(&format!("SELECT {REQUEST_COLUMNS} WHERE r.id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// The request, if `token` is the one from its notification.
pub async fn request_by_action_token(db: &SqlitePool, id: i64, token: &str) -> Result<Option<RequestRow>> {
    Ok(sqlx::query_as(&format!("SELECT {REQUEST_COLUMNS} WHERE r.id = ? AND r.action_token_hash = ?"))
        .bind(id)
        .bind(hash_token(token))
        .fetch_optional(db)
        .await?)
}

pub async fn list_requests(db: &SqlitePool, pending_only: bool, limit: i64) -> Result<Vec<RequestRow>> {
    let filter = if pending_only { "WHERE r.status = 'pending'" } else { "" };
    Ok(sqlx::query_as(&format!("SELECT {REQUEST_COLUMNS} {filter} ORDER BY r.id DESC LIMIT ?"))
        .bind(limit)
        .fetch_all(db)
        .await?)
}

/// Grant `minutes` (0 denies) for a pending request, adding the bonus to `today`.
/// Returns `None` if the request was already handled, so each link works once.
pub async fn resolve_request(db: &SqlitePool, id: i64, minutes: u32, today: NaiveDate) -> Result<Option<RequestRow>> {
    let mut tx = db.begin().await?;
    let status = if minutes == 0 { "denied" } else { "granted" };
    let user: Option<i64> = sqlx::query_scalar(
        "UPDATE time_requests SET status = ?, granted_minutes = ?, resolved_at = ?
         WHERE id = ? AND status = 'pending' RETURNING user_id",
    )
    .bind(status)
    .bind(minutes)
    .bind(now())
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else { return Ok(None) };
    if minutes > 0 {
        sqlx::query("INSERT INTO bonus_grants (user_id, day, minutes, source, granted_at) VALUES (?, ?, ?, ?, ?)")
            .bind(user)
            .bind(today)
            .bind(minutes)
            .bind(format!("request {id}"))
            .bind(now())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    get_request(db, id).await
}

/// Register a machine, or replace its token. Returns the new token, shown once.
pub async fn add_client(db: &SqlitePool, hostname: &str) -> Result<String> {
    let token = new_token();
    sqlx::query(
        "INSERT INTO clients (hostname, token_hash) VALUES (?, ?)
         ON CONFLICT (hostname) DO UPDATE SET token_hash = excluded.token_hash",
    )
    .bind(hostname)
    .bind(hash_token(&token))
    .execute(db)
    .await?;
    Ok(token)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, sqlx::FromRow)]
pub struct AccountRow {
    pub name: String,
    pub admin: bool,
    /// The kid's username, or None for not limited.
    pub kid: Option<String>,
    /// Reported but never assigned by a parent.
    pub new: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ClientRow {
    pub hostname: String,
    pub last_seen: Option<String>,
    pub accounts: Vec<AccountRow>,
}

pub async fn list_clients(db: &SqlitePool) -> Result<Vec<ClientRow>> {
    let rows: Vec<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, hostname, last_seen FROM clients ORDER BY hostname").fetch_all(db).await?;
    let mut out = Vec::new();
    for (id, hostname, last_seen) in rows {
        let accounts = sqlx::query_as(
            "SELECT a.name, a.is_admin AS admin, u.username AS kid, (a.decided = 0 AND a.is_admin = 0) AS new
             FROM accounts a LEFT JOIN users u ON u.id = a.user_id
             WHERE a.client_id = ? ORDER BY a.is_admin, a.name",
        )
        .bind(id)
        .fetch_all(db)
        .await?;
        out.push(ClientRow { hostname, last_seen, accounts });
    }
    Ok(out)
}

/// Record the accounts a computer reports (dropping ones it no longer has) and return which of
/// them are limited, and as which kid.
pub async fn report_accounts(
    db: &SqlitePool,
    client: i64,
    accounts: &[screentime_common::LocalAccount],
    default_minutes: u32,
) -> Result<Vec<screentime_common::AccountMapping>> {
    let now = now();
    for a in accounts {
        let existed: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE client_id = ? AND name = ?")
            .bind(client)
            .bind(&a.name)
            .fetch_optional(db)
            .await?;
        match existed {
            Some(id) => {
                sqlx::query("UPDATE accounts SET is_admin = ?, last_seen = ? WHERE id = ?")
                    .bind(a.admin)
                    .bind(&now)
                    .bind(id)
                    .execute(db)
                    .await?;
            }
            None => {
                // The first time an account shows up, keep what the computer was set up with.
                let kid = match (&a.configured_kid, a.admin) {
                    (Some(kid), false) => Some(ensure_user(db, kid, default_minutes).await?),
                    _ => None,
                };
                sqlx::query(
                    "INSERT INTO accounts (client_id, name, is_admin, user_id, decided, first_seen, last_seen)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(client)
                .bind(&a.name)
                .bind(a.admin)
                .bind(kid)
                .bind(kid.is_some())
                .bind(&now)
                .bind(&now)
                .execute(db)
                .await?;
            }
        }
    }
    let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM accounts WHERE client_id = ?")
        .bind(client)
        .fetch_all(db)
        .await?;
    for (id, name) in rows {
        if !accounts.iter().any(|a| a.name.eq_ignore_ascii_case(&name)) {
            sqlx::query("DELETE FROM accounts WHERE id = ?").bind(id).execute(db).await?;
        }
    }
    let limited: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.name, u.username FROM accounts a JOIN users u ON u.id = a.user_id
         WHERE a.client_id = ? AND a.is_admin = 0 ORDER BY a.name",
    )
    .bind(client)
    .fetch_all(db)
    .await?;
    Ok(limited.into_iter().map(|(account, kid)| screentime_common::AccountMapping { account, kid }).collect())
}

/// Assign a computer's account to a kid (by username), or to nobody.
pub async fn set_account(db: &SqlitePool, hostname: &str, account: &str, kid: Option<i64>) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE accounts SET user_id = ?, decided = 1
         WHERE name = ? AND is_admin = 0 AND client_id = (SELECT id FROM clients WHERE hostname = ?)",
    )
    .bind(kid)
    .bind(account)
    .bind(hostname)
    .execute(db)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn set_display_name(db: &SqlitePool, user: i64, name: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE users SET display_name = ? WHERE id = ?").bind(name).bind(user).execute(db).await?;
    Ok(())
}

pub async fn display_name(db: &SqlitePool, user: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT display_name FROM users WHERE id = ?").bind(user).fetch_one(db).await?)
}

pub async fn remove_client(db: &SqlitePool, hostname: &str) -> Result<bool> {
    let r = sqlx::query("DELETE FROM clients WHERE hostname = ?").bind(hostname).execute(db).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn create_session(db: &SqlitePool, idle: chrono::TimeDelta) -> Result<String> {
    let token = new_token();
    let created = Utc::now();
    sqlx::query("INSERT INTO sessions (token_hash, created_at, expires_at) VALUES (?, ?, ?)")
        .bind(hash_token(&token))
        .bind(created.to_rfc3339())
        .bind((created + idle).to_rfc3339())
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?").bind(created.to_rfc3339()).execute(db).await?;
    Ok(token)
}

const ADMIN_HASH_SETTING: &str = "admin_password_hash";

/// The admin password hash set from the web UI, if it has been changed there.
pub async fn admin_password_hash(db: &SqlitePool) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(ADMIN_HASH_SETTING).fetch_optional(db).await?)
}

pub async fn set_admin_password_hash(db: &SqlitePool, hash: &str) -> Result<()> {
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
        .bind(ADMIN_HASH_SETTING)
        .bind(hash)
        .execute(db)
        .await?;
    Ok(())
}


pub async fn delete_all_sessions(db: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM sessions").execute(db).await?;
    Ok(())
}

pub async fn delete_session(db: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?").bind(hash_token(token)).execute(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekday_mask_round_trip() {
        let days = vec![Weekday::Mon, Weekday::Wed, Weekday::Sun];
        assert_eq!(days_to_mask(&days), 0b1000101);
        assert_eq!(mask_to_days(days_to_mask(&days)), days);
    }
}
