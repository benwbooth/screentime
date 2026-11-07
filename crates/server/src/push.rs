//! Web Push straight from the server to the parent's browser, no extra app or service.
//! Keys are VAPID (RFC 8292); payloads are encrypted per RFC 8291 by `web-push-native`.

use crate::db::RequestRow;
use anyhow::{Context, Result};
use base64ct::{Base64UrlUnpadded, Encoding};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use web_push_native::jwt_simple::algorithms::ES256KeyPair;
use web_push_native::{Auth, WebPushBuilder, p256::PublicKey};

const VAPID_SETTING: &str = "vapid_private_key";

/// The server's VAPID key pair, created and saved on first use.
pub async fn vapid_keypair(db: &SqlitePool) -> Result<ES256KeyPair> {
    let saved: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(VAPID_SETTING)
        .fetch_optional(db)
        .await?;
    if let Some(saved) = saved {
        let bytes = Base64UrlUnpadded::decode_vec(&saved).map_err(|e| anyhow::anyhow!("{e}"))?;
        return ES256KeyPair::from_bytes(&bytes).map_err(|e| anyhow::anyhow!("{e}"));
    }
    let kp = ES256KeyPair::generate();
    sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, ?)")
        .bind(VAPID_SETTING)
        .bind(Base64UrlUnpadded::encode_string(&kp.to_bytes()))
        .execute(db)
        .await?;
    // Another request may have won the race; use whatever got saved.
    Box::pin(vapid_keypair(db)).await
}

/// The public key browsers need as `applicationServerKey`, base64url.
pub async fn public_key(db: &SqlitePool) -> Result<String> {
    use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint;
    let kp = vapid_keypair(db).await?;
    let secret = web_push_native::p256::SecretKey::from_slice(&kp.to_bytes())?;
    Ok(Base64UrlUnpadded::encode_string(secret.public_key().to_encoded_point(false).as_bytes()))
}

/// A browser push subscription, as `PushSubscription.toJSON()` gives it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

pub async fn subscribe(db: &SqlitePool, sub: &Subscription) -> Result<()> {
    anyhow::ensure!(sub.endpoint.starts_with("https://"), "push endpoint must be https");
    // Reject keys that won't decode now rather than at send time.
    builder_for(&sub.endpoint, &sub.keys.p256dh, &sub.keys.auth)?;
    sqlx::query(
        "INSERT INTO push_subscriptions (endpoint, p256dh, auth, label, created_at) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth, label = excluded.label",
    )
    .bind(&sub.endpoint)
    .bind(&sub.keys.p256dh)
    .bind(&sub.keys.auth)
    .bind(&sub.label)
    .bind(Utc::now().to_rfc3339())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn unsubscribe(db: &SqlitePool, endpoint: &str) -> Result<()> {
    sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = ?").bind(endpoint).execute(db).await?;
    Ok(())
}

pub async fn subscription_count(db: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM push_subscriptions").fetch_one(db).await?)
}

fn builder_for(endpoint: &str, p256dh: &str, auth: &str) -> Result<WebPushBuilder> {
    let key = Base64UrlUnpadded::decode_vec(p256dh).map_err(|e| anyhow::anyhow!("p256dh: {e}"))?;
    let auth = Base64UrlUnpadded::decode_vec(auth).map_err(|e| anyhow::anyhow!("auth: {e}"))?;
    anyhow::ensure!(auth.len() == 16, "auth secret must be 16 bytes");
    Ok(WebPushBuilder::new(
        endpoint.parse().context("endpoint")?,
        PublicKey::from_sec1_bytes(&key).context("p256dh")?,
        Auth::clone_from_slice(&auth),
    ))
}

/// What the service worker turns into a notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Payload {
    pub title: String,
    pub body: String,
    /// Opened when the notification itself is tapped.
    pub url: String,
    /// Notifications with the same tag replace each other: one per kid's request.
    #[serde(default)]
    pub tag: Option<String>,
    /// Buttons; the service worker POSTs to the button's URL. Android shows up to two.
    pub actions: Vec<PayloadAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PayloadAction {
    pub title: String,
    pub url: String,
}

pub fn request_payload(row: &RequestRow, page_url: String, quick: &[(u32, String)]) -> Payload {
    let asked = match row.requested_minutes {
        Some(m) => format!("{m} more minutes"),
        None => "more time".to_owned(),
    };
    let mut body = format!("Asking for {asked} on {}.", row.host);
    if let Some(reason) = &row.reason {
        body.push_str(&format!(" “{reason}”"));
    }
    Payload {
        title: format!("{} wants more screen time", row.username),
        body,
        url: page_url,
        tag: Some(format!("request-{}", row.username)),
        actions: quick.iter().map(|(m, url)| PayloadAction { title: format!("+{m} min"), url: url.clone() }).collect(),
    }
}

/// Send `payload` to every subscribed browser, dropping subscriptions the push service says are gone.
/// Returns how many deliveries were accepted.
pub async fn send_all(db: &SqlitePool, http: &reqwest::Client, contact: &str, payload: &Payload) -> Result<usize> {
    let subs: Vec<(i64, String, String, String)> =
        sqlx::query_as("SELECT id, endpoint, p256dh, auth FROM push_subscriptions").fetch_all(db).await?;
    if subs.is_empty() {
        return Ok(0);
    }
    let body = serde_json::to_vec(payload)?;
    // Build every request up front: the key pair isn't Send, so it must not live across an await.
    let built: Vec<(i64, Result<http::Request<Vec<u8>>>)> = {
        let kp = vapid_keypair(db).await?;
        subs.into_iter()
            .map(|(id, endpoint, p256dh, auth)| {
                let req = builder_for(&endpoint, &p256dh, &auth).and_then(|b| {
                    b.with_vapid(&kp, contact).build(body.clone()).map_err(|e| anyhow::anyhow!("{e}"))
                });
                (id, req)
            })
            .collect()
    };
    let mut delivered = 0;
    for (id, request) in built {
        let result = async {
            let (parts, body) = request?.into_parts();
            let mut req = http.post(parts.uri.to_string()).body(body).header("Urgency", "high");
            for (name, value) in parts.headers.iter() {
                req = req.header(name, value);
            }
            Ok::<_, anyhow::Error>(req.send().await?)
        }
        .await;
        match result {
            Ok(resp) if resp.status().is_success() => delivered += 1,
            Ok(resp) if matches!(resp.status().as_u16(), 404 | 410) => {
                tracing::info!(id, "push subscription expired, removing");
                sqlx::query("DELETE FROM push_subscriptions WHERE id = ?").bind(id).execute(db).await?;
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(id, %status, %body, "push rejected");
            }
            Err(e) => tracing::warn!(id, "push failed: {e:#}"),
        }
    }
    Ok(delivered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn vapid_key_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::open_db(&format!("sqlite://{}", dir.path().join("t.db").display())).await.unwrap();
        let a = public_key(&db).await.unwrap();
        let b = public_key(&db).await.unwrap();
        assert_eq!(a, b);
        // An uncompressed P-256 point is 65 bytes.
        assert_eq!(Base64UrlUnpadded::decode_vec(&a).unwrap().len(), 65);
    }
}
