//! The page and one-tap links from a phone notification. Each request carries its own random
//! token, so these work without signing in, only for that request, and only once.

use crate::error::{AppError, AppResult};
use crate::{AppState, db};
use axum::extract::{Form, Path, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use axum::Router;
use chrono::Local;
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/r/{id}/{token}", get(page))
        .route("/act/{id}/{token}", post(act_custom))
        .route("/act/{id}/{token}/{minutes}", post(act))
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

const STYLE: &str = "
:root { color-scheme: light dark; --bg: #fafaf9; --fg: #1c1917; --muted: #78716c; --accent: #2563eb; --card: #fff; --line: #e7e5e4; }
@media (prefers-color-scheme: dark) { :root { --bg: #1c1917; --fg: #f5f5f4; --muted: #a8a29e; --accent: #60a5fa; --card: #292524; --line: #44403c; } }
body { margin: 0; background: var(--bg); color: var(--fg); font: 17px/1.45 system-ui, sans-serif; }
main { max-width: 28rem; margin: 0 auto; padding: 24px 16px; }
h1 { font-size: 1.35rem; margin: 0 0 4px; }
p.muted { color: var(--muted); margin: 0 0 20px; }
blockquote { margin: 0 0 20px; padding: 12px 14px; background: var(--card); border: 1px solid var(--line); border-radius: 10px; }
.grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; margin-bottom: 14px; }
button { font: inherit; padding: 14px 0; border-radius: 10px; border: 1px solid var(--line); background: var(--card); color: var(--fg); width: 100%; }
button.primary { background: var(--accent); border-color: var(--accent); color: #fff; font-weight: 600; }
form.custom { display: flex; gap: 10px; margin-bottom: 14px; }
form.custom input { flex: 1; font: inherit; padding: 12px; border-radius: 10px; border: 1px solid var(--line); background: var(--card); color: var(--fg); min-width: 0; }
form.custom button { width: auto; padding: 0 18px; }
a.back { display: block; text-align: center; margin-top: 24px; padding: 14px 0; border-radius: 10px; border: 1px solid var(--line); color: var(--accent); text-decoration: none; font-weight: 600; }
";

fn layout(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style></head><body><main>{body}\
         <a class=back href=\"/\">Open Screen time</a></main></body></html>",
        escape(title)
    ))
}

async fn load(s: &AppState, id: i64, token: &str) -> AppResult<db::RequestRow> {
    db::request_by_action_token(&s.db, id, token).await?.ok_or(AppError::NotFound)
}

async fn page(State(s): State<AppState>, Path((id, token)): Path<(i64, String)>) -> AppResult<Html<String>> {
    let r = load(&s, id, &token).await?;
    let user = escape(&r.username);
    let asked = match r.requested_minutes {
        Some(m) => format!("asked for {m} more minutes"),
        None => "asked for more time".to_owned(),
    };
    let mut body = format!("<h1>{user} wants more screen time</h1><p class=muted>{asked} on {}</p>", escape(&r.host));
    if let Some(reason) = &r.reason {
        body.push_str(&format!("<blockquote>{}</blockquote>", escape(reason)));
    }
    if r.status != "pending" {
        body.push_str(&resolved_text(&r));
        return Ok(layout("Time request", &body));
    }
    let action = |m: u32| format!("/act/{id}/{token}/{m}");
    body.push_str("<div class=grid>");
    for m in [15, 30, 45, 60] {
        let class = if r.requested_minutes == Some(m.into()) { " class=primary" } else { "" };
        body.push_str(&format!(
            "<form method=post action=\"{}\"><button{class}>+{m} min</button></form>",
            action(m)
        ));
    }
    body.push_str(&format!(
        "<form method=post action=\"{}\"><button>No</button></form></div>\
         <form class=custom method=post action=\"/act/{id}/{token}\">\
         <input name=minutes type=number min=1 max=1440 inputmode=numeric placeholder=\"Custom minutes\" required>\
         <button class=primary>Add</button></form>",
        action(0)
    ));
    Ok(layout("Time request", &body))
}

fn resolved_text(r: &db::RequestRow) -> String {
    match (r.status.as_str(), r.granted_minutes) {
        ("granted", Some(m)) => format!("<p>Added {m} minutes for {}.</p>", escape(&r.username)),
        ("replaced", _) => format!("<p>{} asked again since; answer the newer request.</p>", escape(&r.username)),
        _ => format!("<p>Request declined for {}.</p>", escape(&r.username)),
    }
}

async fn resolve(s: &AppState, id: i64, token: &str, minutes: u32) -> AppResult<(StatusCode, Html<String>)> {
    if minutes > 24 * 60 {
        return Err(AppError::BadRequest("at most 24 hours".into()));
    }
    let existing = load(s, id, token).await?;
    let today = Local::now().date_naive();
    // A notification button that didn't add anything says so (409), instead of "Done".
    let (status, body) = match db::resolve_request(&s.db, id, minutes, today).await? {
        Some(r) => {
            tracing::info!(id, user = %r.username, minutes, "request answered from link");
            (StatusCode::OK, resolved_text(&r))
        }
        None => (StatusCode::CONFLICT, format!("<p>This request was already answered.</p>{}", resolved_text(&existing))),
    };
    Ok((status, layout("Time request", &body)))
}

async fn act(
    State(s): State<AppState>,
    Path((id, token, minutes)): Path<(i64, String, u32)>,
) -> AppResult<(StatusCode, Html<String>)> {
    resolve(&s, id, &token, minutes).await
}

#[derive(Deserialize)]
struct CustomForm {
    minutes: u32,
}

async fn act_custom(
    State(s): State<AppState>,
    Path((id, token)): Path<(i64, String)>,
    Form(f): Form<CustomForm>,
) -> AppResult<(StatusCode, Html<String>)> {
    resolve(&s, id, &token, f.minutes).await
}
