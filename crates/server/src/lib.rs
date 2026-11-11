//! screentime server: stores screen time settings in SQLite, serves the client daemons
//! and the admin web UI, and sends phone notifications for time requests.

pub mod admin;
pub mod auth;
pub mod certs;
pub mod client_api;
pub mod config;
pub mod db;
pub mod error;
pub mod notify;
pub mod push;
pub mod request_page;
pub mod script;
pub mod wifi;

use axum::Router;
use axum::http::{HeaderValue, header};
use config::Config;
use notify::Notifier;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<Config>,
    pub notifier: Arc<Notifier>,
    /// Recent failed admin logins, for throttling.
    pub login_failures: Arc<Mutex<Vec<Instant>>>,
    pub wifi: Arc<wifi::Wifi>,
}

impl AppState {
    pub fn new(db: SqlitePool, config: Config) -> AppState {
        let notifier = Notifier::new(&config);
        AppState {
            db,
            config: Arc::new(config),
            notifier: Arc::new(notifier),
            login_failures: Arc::new(Mutex::new(Vec::new())),
            wifi: Arc::new(wifi::Wifi::default()),
        }
    }
}

/// Response header carrying the web UI's version (its ETag).
pub const UI_VERSION_HEADER: &str = "x-screentime-ui";

const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

pub fn router(state: AppState) -> Router {
    use axum::http::HeaderName;
    use tower_http::set_header::SetResponseHeaderLayer;

    let web_dir = state.config.web_dir.clone();
    let mut app = Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(client_api::routes())
        .merge(admin::routes())
        .merge(request_page::routes())
        .with_state(state);
    let mut ui_version = None;
    match web_dir {
        Some(dir) => {
            let index = tower_http::services::ServeFile::new(dir.join("index.html"));
            let etag = web_etag(&dir);
            ui_version = Some(etag.clone());
            let files = tower_http::services::ServeDir::new(&dir).fallback(index);
            app = app.fallback_service(
                tower::ServiceBuilder::new()
                    .layer(axum::middleware::from_fn(move |req, next| revalidate(etag.clone(), req, next)))
                    .service(files),
            );
        }
        None => tracing::warn!("no web UI found; set SCREENTIME_WEB_DIR to serve it"),
    }
    let header = |name: HeaderName, value: &'static str| {
        SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
    };
    // Every reply names the web UI it goes with, so a page left open across an upgrade notices
    // and reloads instead of misreading the new API.
    if let Some(v) = ui_version {
        app = app.layer(SetResponseHeaderLayer::overriding(HeaderName::from_static(UI_VERSION_HEADER), v));
    }
    app.layer(header(header::CONTENT_SECURITY_POLICY, CSP))
        .layer(header(header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
        // Notification links carry tokens in the path, so never leak them in a Referer.
        .layer(header(header::REFERRER_POLICY, "no-referrer"))
        .layer(header(header::CACHE_CONTROL, "no-cache"))
        .layer(tower_http::compression::CompressionLayer::new())
}

/// One tag for the whole web UI, from its files' contents. Files in the nix store all date from
/// 1970, so revalidating by date would keep a phone on an old copy of the app forever.
fn web_etag(dir: &std::path::Path) -> HeaderValue {
    use sha2::{Digest, Sha256};
    let mut paths: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    paths.sort();
    let mut hash = Sha256::new();
    for path in paths {
        hash.update(path.file_name().unwrap_or_default().as_encoded_bytes());
        hash.update(std::fs::read(&path).unwrap_or_default());
    }
    HeaderValue::from_str(&format!("\"{}\"", &hex::encode(hash.finalize())[..16])).expect("hex is a valid header")
}

async fn revalidate(etag: HeaderValue, mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.headers().get(header::IF_NONE_MATCH) == Some(&etag) {
        return (axum::http::StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    req.headers_mut().remove(header::IF_MODIFIED_SINCE);
    let mut res = next.run(req).await;
    res.headers_mut().remove(header::LAST_MODIFIED);
    if res.status().is_success() {
        res.headers_mut().insert(header::ETAG, etag);
    }
    res
}

/// Open (creating if needed) the database and bring its schema up to date.
pub async fn open_db(url: &str) -> anyhow::Result<SqlitePool> {
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::str::FromStr;
    let options = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new().connect_with(options).await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}
