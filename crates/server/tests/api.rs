use screentime_common::{DowntimeWindow, Policy, UsageReport};
use screentime_server::{AppState, auth, config::Config, db, open_db, router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{Local, NaiveTime, Weekday};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

struct TestServer {
    app: Router,
    state: AppState,
    _dir: tempfile::TempDir,
}

async fn server(config: Config) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let pool = open_db(&format!("sqlite://{}", dir.path().join("t.db").display())).await.unwrap();
    let state = AppState::new(pool, Config { default_allowance_minutes: 120, ..config });
    TestServer { app: router(state.clone()), state, _dir: dir }
}

impl TestServer {
    async fn call(&self, method: &str, uri: &str, auth: Option<&str>, body: Option<Value>) -> (StatusCode, Vec<(String, String)>, String) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(a) = auth {
            req = if a.starts_with("screentime_session=") {
                req.header(header::COOKIE, a)
            } else {
                req.header(header::AUTHORIZATION, format!("Bearer {a}"))
            };
        }
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = self.app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned()))
            .collect();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn policy(&self, token: &str, user: &str) -> Policy {
        let (status, _, body) = self.call("GET", &format!("/api/client/policy/{user}"), Some(token), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn admin_cookie(&self) -> String {
        let (status, headers, _) =
            self.call("POST", "/api/login", None, Some(json!({"password": "correct horse"}))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let cookie = headers.iter().find(|(k, _)| k == "set-cookie").unwrap().1.clone();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        cookie.split(';').next().unwrap().to_owned()
    }
}

fn with_password() -> Config {
    Config { admin_password_hash: Some(auth::hash_password("correct horse").unwrap()), ..Config::default() }
}

#[tokio::test]
async fn client_api_needs_a_valid_token() {
    let s = server(Config::default()).await;
    assert_eq!(s.call("GET", "/api/client/policy/kid", None, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(s.call("GET", "/api/client/policy/kid", Some("nope"), None).await.0, StatusCode::UNAUTHORIZED);
    let token = db::add_client(&s.state.db, "pc1").await.unwrap();
    assert_eq!(s.call("GET", "/api/client/policy/kid", Some(&token), None).await.0, StatusCode::OK);
    assert_eq!(s.call("GET", "/api/client/policy/..%2Fx", Some(&token), None).await.0, StatusCode::BAD_REQUEST);
    // Re-registering a machine revokes its old token.
    let new = db::add_client(&s.state.db, "pc1").await.unwrap();
    assert_eq!(s.call("GET", "/api/client/policy/kid", Some(&token), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(s.call("GET", "/api/client/policy/kid", Some(&new), None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn allowance_is_shared_across_machines() {
    let s = server(Config::default()).await;
    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let pc2 = db::add_client(&s.state.db, "pc2").await.unwrap();
    let today = Local::now().date_naive();

    let p = s.policy(&pc1, "kid").await;
    assert_eq!((p.daily_allowance_minutes, p.bonus_minutes, p.day), (Some(120), 0, today));

    let report = UsageReport { username: "kid".into(), host: "ignored".into(), day: today, active_minutes: 50, local_bonus_minutes: 0 };
    let (status, _, _) = s.call("POST", "/api/client/usage", Some(&pc1), Some(serde_json::to_value(&report).unwrap())).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // pc1 counts its own 50 minutes itself; pc2 sees them taken off the allowance.
    assert_eq!(s.policy(&pc1, "kid").await.daily_allowance_minutes, Some(120));
    assert_eq!(s.policy(&pc2, "kid").await.daily_allowance_minutes, Some(70));
}

#[tokio::test]
async fn earned_bonus_time_is_shared_across_machines_too() {
    // A 0-minute limit plus 30 minutes earned: 20 used on pc1 leaves 10 on pc2, not 30.
    let s = server(Config::default()).await;
    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let pc2 = db::add_client(&s.state.db, "pc2").await.unwrap();
    let today = Local::now().date_naive();
    let uid = db::ensure_user(&s.state.db, "kid", 0).await.unwrap();
    db::add_bonus(&s.state.db, uid, today, 30, "web").await.unwrap();
    let report = UsageReport { username: "kid".into(), host: "x".into(), day: today, active_minutes: 20, local_bonus_minutes: 0 };
    s.call("POST", "/api/client/usage", Some(&pc1), Some(serde_json::to_value(&report).unwrap())).await;

    let p2 = s.policy(&pc2, "kid").await;
    assert_eq!(p2.daily_allowance_minutes.unwrap() + p2.bonus_minutes, 10);
    let p1 = s.policy(&pc1, "kid").await;
    assert_eq!(p1.daily_allowance_minutes.unwrap() + p1.bonus_minutes, 30);
}

#[tokio::test]
async fn time_a_parent_added_at_a_computer_counts_everywhere() {
    // Ryan's evening: 15 minutes approved at pc1, 11 used there, then 45 added from the web.
    let s = server(Config { default_allowance_minutes: 0, ..with_password() }).await;
    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let pc2 = db::add_client(&s.state.db, "pc2").await.unwrap();
    let today = Local::now().date_naive();
    let uid = db::ensure_user(&s.state.db, "kid", 0).await.unwrap();
    let report = UsageReport { username: "kid".into(), host: "x".into(), day: today, active_minutes: 11, local_bonus_minutes: 15 };
    s.call("POST", "/api/client/usage", Some(&pc1), Some(serde_json::to_value(&report).unwrap())).await;
    db::add_bonus(&s.state.db, uid, today, 45, "web").await.unwrap();

    let total = |p: Policy| p.daily_allowance_minutes.unwrap() + p.bonus_minutes;
    // pc1 adds its own 15 and takes off its own 11 itself.
    assert_eq!(total(s.policy(&pc1, "kid").await), 45);
    assert_eq!(total(s.policy(&pc2, "kid").await), 45 + 15 - 11);
    let me = s.admin_cookie().await;
    let overview: Value = serde_json::from_str(&s.call("GET", "/api/overview", Some(me.as_str()), None).await.2).unwrap();
    let kid = overview["users"].as_array().unwrap().iter().find(|u| u["username"] == "kid").unwrap();
    assert_eq!(kid["decision"]["remaining_minutes"], 49, "{kid}");
}

#[tokio::test]
async fn notification_link_grants_once() {
    let s = server(Config::default()).await;
    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let uid = db::ensure_user(&s.state.db, "kid", 120).await.unwrap();
    let (row, token) = db::create_request(&s.state.db, uid, "pc1", Some(30), Some("<b>homework</b>")).await.unwrap();

    let (status, _, page) = s.call("GET", &format!("/r/{}/{token}", row.id), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("&lt;b&gt;homework&lt;/b&gt;") && !page.contains("<b>homework"));
    assert!(page.contains("+45 min") && page.contains("Custom minutes"));
    assert!(page.contains("href=\"/\">Open Screen time"), "the page needs a way back to the app");

    assert_eq!(s.call("GET", &format!("/r/{}/wrong", row.id), None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(s.call("POST", &format!("/act/{}/wrong/30", row.id), None, None).await.0, StatusCode::NOT_FOUND);

    let (status, _, body) = s.call("POST", &format!("/act/{}/{token}/30", row.id), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Added 30 minutes"));
    assert_eq!(s.policy(&pc1, "kid").await.bonus_minutes, 30);

    let (_, _, body) = s.call("POST", &format!("/act/{}/{token}/60", row.id), None, None).await;
    assert!(body.contains("already answered"));
    assert_eq!(s.policy(&pc1, "kid").await.bonus_minutes, 30);
}

#[tokio::test]
async fn admin_login_and_settings() {
    let s = server(with_password()).await;
    assert_eq!(s.call("GET", "/api/overview", None, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        s.call("GET", "/api/overview", Some("screentime_session=forged"), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        s.call("POST", "/api/login", None, Some(json!({"password": "wrong"}))).await.0,
        StatusCode::UNAUTHORIZED
    );
    let cookie = s.admin_cookie().await;
    let admin = Some(cookie.as_str());

    assert_eq!(s.call("POST", "/api/users", admin, Some(json!({"username": "kid"}))).await.0, StatusCode::CREATED);
    let allowances = json!([60, 60, 60, 60, 90, 180, null]);
    assert_eq!(s.call("PUT", "/api/users/kid/allowances", admin, Some(allowances)).await.0, StatusCode::NO_CONTENT);
    let downtime = vec![DowntimeWindow {
        days: vec![Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Sun],
        start: NaiveTime::from_hms_opt(21, 0, 0).unwrap(),
        end: NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
    }];
    let (status, _, _) = s.call("PUT", "/api/users/kid/downtime", admin, Some(serde_json::to_value(&downtime).unwrap())).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(s.call("POST", "/api/users/kid/bonus", admin, Some(json!({"minutes": 45}))).await.0, StatusCode::NO_CONTENT);

    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let p = s.policy(&pc1, "kid").await;
    let expected = [Some(60), Some(60), Some(60), Some(60), Some(90), Some(180), None];
    use chrono::Datelike;
    assert_eq!(p.daily_allowance_minutes, expected[Local::now().weekday().num_days_from_monday() as usize]);
    assert_eq!(p.bonus_minutes, 45);
    assert_eq!(p.downtime, downtime);

    // A request from the machine shows up for the admin and can be answered there.
    let (status, _, _) = s
        .call("POST", "/api/client/requests", Some(&pc1), Some(json!({"username": "kid", "host": "x", "requested_minutes": 15, "reason": null})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, _, body) = s.call("GET", "/api/overview", admin, None).await;
    let overview: Value = serde_json::from_str(&body).unwrap();
    let pending = &overview["pending_requests"][0];
    assert_eq!((pending["username"].as_str(), pending["host"].as_str()), (Some("kid"), Some("pc1")));
    let id = pending["id"].as_i64().unwrap();
    let (status, _, _) = s.call("POST", &format!("/api/requests/{id}/resolve"), admin, Some(json!({"minutes": 0}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        s.call("POST", &format!("/api/requests/{id}/resolve"), admin, Some(json!({"minutes": 15}))).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(s.policy(&pc1, "kid").await.bonus_minutes, 45);

    // Machines can be added from the UI, and logging out ends the session.
    let (status, _, body) = s.call("POST", "/api/clients", admin, Some(json!({"hostname": "pc2"}))).await;
    assert_eq!(status, StatusCode::OK);
    let token: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(s.policy(token["token"].as_str().unwrap(), "kid").await.username, "kid");
    assert_eq!(s.call("POST", "/api/logout", admin, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(s.call("GET", "/api/overview", admin, None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_is_throttled() {
    let s = server(with_password()).await;
    for _ in 0..5 {
        assert_eq!(s.call("POST", "/api/login", None, Some(json!({"password": "nope"}))).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        s.call("POST", "/api/login", None, Some(json!({"password": "correct horse"}))).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn no_password_means_no_login() {
    let s = server(Config::default()).await;
    assert_eq!(
        s.call("POST", "/api/login", None, Some(json!({"password": ""}))).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

/// A stand-in ntfy server: publishing stores a message on a topic, and `/{topic}/json` returns
/// the stored messages as JSON lines and closes, which the server treats as a reconnect.
/// (topic, headers, body) for each published message.
type Published = Vec<(String, Vec<(String, String)>, String)>;

#[derive(Clone, Default)]
struct FakeNtfy {
    topics: Arc<Mutex<Published>>,
}

impl FakeNtfy {
    async fn start(&self) -> std::net::SocketAddr {
        let publish = self.clone();
        let subscribe = self.clone();
        let app = Router::new()
            .route(
                "/{topic}",
                axum::routing::post(move |axum::extract::Path(topic): axum::extract::Path<String>, headers: axum::http::HeaderMap, body: String| {
                    let me = publish.clone();
                    async move {
                        let h = headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned())).collect();
                        me.topics.lock().unwrap().push((topic, h, body));
                        "{}"
                    }
                }),
            )
            .route(
                "/{topic}/json",
                axum::routing::get(move |axum::extract::Path(topic): axum::extract::Path<String>| {
                    let me = subscribe.clone();
                    async move {
                        let topics = me.topics.lock().unwrap();
                        topics
                            .iter()
                            .enumerate()
                            .filter(|(_, (t, _, _))| *t == topic)
                            .map(|(i, (_, _, body))| json!({"id": format!("m{i}"), "event": "message", "message": body}).to_string() + "\n")
                            .collect::<String>()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    fn on(&self, topic: &str) -> Vec<(Vec<(String, String)>, String)> {
        self.topics.lock().unwrap().iter().filter(|(t, _, _)| t == topic).map(|(_, h, b)| (h.clone(), b.clone())).collect()
    }

    async fn wait_for(&self, topic: &str, count: usize) {
        for _ in 0..200 {
            if self.on(topic).len() >= count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {count} message(s) on {topic}");
    }
}

#[tokio::test]
async fn ntfy_buttons_answer_through_ntfy() {
    let ntfy = FakeNtfy::default();
    let addr = ntfy.start().await;
    let s = server(Config {
        ntfy_url: Some(format!("http://{addr}/topic")),
        ntfy_token: Some("tk".into()),
        public_url: "https://screentime.example".into(),
        ..Config::default()
    })
    .await;
    tokio::spawn(screentime_server::notify::follow_replies(s.state.clone()));
    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let (status, _, _) = s
        .call("POST", "/api/client/requests", Some(&pc1), Some(json!({"username": "kid", "host": "pc1", "requested_minutes": 45, "reason": "finish the level"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    ntfy.wait_for("topic", 1).await;

    let (headers, body) = ntfy.on("topic")[0].clone();
    let get = |k: &str| headers.iter().find(|(h, _)| h == k).map(|(_, v)| v.clone()).unwrap_or_default();
    assert_eq!(get("title"), "kid wants more screen time");
    assert_eq!(get("authorization"), "Bearer tk");
    assert!(body.contains("45 more minutes on pc1") && body.contains("finish the level"), "{body}");
    assert!(get("click").starts_with("https://screentime.example/r/1/"));
    // Three buttons, each posting back to the answers topic, never to the server itself.
    let actions = get("actions");
    let buttons: Vec<&str> = actions.split("; ").collect();
    assert_eq!(buttons.len(), 3, "{actions}");
    assert!(buttons[0].starts_with(&format!("http, +45 min, http://{addr}/topic-answers, method=POST, body=grant:1:")), "{actions}");
    assert!(buttons[0].contains("headers.Authorization=Bearer tk") && !actions.contains("screentime.example"), "{actions}");

    // A forged answer is ignored; the phone tapping "+45 min" grants it.
    let real = buttons[0].split("body=").nth(1).unwrap().split(',').next().unwrap().to_owned();
    let client = reqwest::Client::new();
    client.post(format!("http://{addr}/topic-answers")).body("grant:1:not-the-token:600").send().await.unwrap();
    client.post(format!("http://{addr}/topic-answers")).body(real.clone()).send().await.unwrap();
    ntfy.wait_for("topic", 2).await;
    assert_eq!(s.policy(&pc1, "kid").await.bonus_minutes, 45);
    let (confirm_headers, _) = ntfy.on("topic")[1].clone();
    assert!(confirm_headers.iter().any(|(k, v)| k == "title" && v == "Gave kid 45 more minutes"));

    // Replaying the same answer does nothing more.
    client.post(format!("http://{addr}/topic-answers")).body(real).send().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(s.policy(&pc1, "kid").await.bonus_minutes, 45);
    assert_eq!(ntfy.on("topic").len(), 2);
}

#[tokio::test]
async fn request_sends_encrypted_web_push_with_grant_buttons() {
    use base64ct::{Base64UrlUnpadded, Encoding};
    use web_push_native::p256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};

    // A stand-in push service (what FCM or Apple would be) recording what it receives.
    type Received = Vec<(Vec<(String, String)>, Vec<u8>)>;
    let seen: Arc<Mutex<Received>> = Arc::default();
    let record = seen.clone();
    let fake = Router::new().route(
        "/push/{id}",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let record = record.clone();
            async move {
                let h = headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned())).collect();
                record.lock().unwrap().push((h, body.to_vec()));
                StatusCode::CREATED
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });

    let s = server(Config { public_url: "https://screentime.example".into(), ..with_password() }).await;
    // The browser's side of the subscription.
    let ua_secret = SecretKey::from_slice(&rand::random::<[u8; 32]>()).unwrap();
    let auth: [u8; 16] = rand::random();
    let p256dh = Base64UrlUnpadded::encode_string(ua_secret.public_key().to_encoded_point(false).as_bytes());
    // Real endpoints must be https; insert directly to point at the local stand-in.
    sqlx::query("INSERT INTO push_subscriptions (endpoint, p256dh, auth, created_at) VALUES (?, ?, ?, 'now')")
        .bind(format!("http://{addr}/push/1"))
        .bind(&p256dh)
        .bind(Base64UrlUnpadded::encode_string(&auth))
        .execute(&s.state.db)
        .await
        .unwrap();

    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let (status, _, _) = s
        .call("POST", "/api/client/requests", Some(&pc1), Some(json!({"username": "kid", "host": "pc1", "requested_minutes": 30, "reason": null})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    for _ in 0..50 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let (headers, body) = seen.lock().unwrap()[0].clone();
    let get = |k: &str| headers.iter().find(|(h, _)| h == k).map(|(_, v)| v.clone()).unwrap_or_default();
    assert_eq!(get("content-encoding"), "aes128gcm");
    let cookie = s.admin_cookie().await;
    let (_, _, info) = s.call("GET", "/api/push", Some(&cookie), None).await;
    let key: Value = serde_json::from_str(&info).unwrap();
    assert!(get("authorization").contains(&format!("k={}", key["public_key"].as_str().unwrap())), "{}", get("authorization"));

    let plain = web_push_native::decrypt(body, &ua_secret, &web_push_native::Auth::clone_from_slice(&auth)).unwrap();
    let payload: Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(payload["title"], "kid wants more screen time");
    assert_eq!(payload["actions"][0]["title"], "+30 min");
    let act = payload["actions"][0]["url"].as_str().unwrap();
    assert!(act.starts_with("https://screentime.example/act/1/"));

    // The notification button's URL grants the time.
    let path = act.trim_start_matches("https://screentime.example");
    assert_eq!(s.call("POST", path, None, None).await.0, StatusCode::OK);
    assert_eq!(s.policy(&pc1, "kid").await.bonus_minutes, 30);
}

#[tokio::test]
async fn change_password_from_the_ui() {
    let s = server(with_password()).await;
    let old_session = s.admin_cookie().await;
    let other_session = s.admin_cookie().await;
    let me = Some(old_session.as_str());

    let change = |current: &str, new: &str| json!({"current": current, "new": new});
    assert_eq!(s.call("POST", "/api/password", None, Some(change("correct horse", "new pass"))).await.0, StatusCode::UNAUTHORIZED);
    let (status, _, body) = s.call("POST", "/api/password", me, Some(change("correct horse", "short"))).await;
    assert_eq!((status, body.as_str()), (StatusCode::BAD_REQUEST, "Use at least 7 characters."));
    let (status, _, body) = s.call("POST", "/api/password", me, Some(change("wrong", "new pass"))).await;
    assert_eq!((status, body.as_str()), (StatusCode::BAD_REQUEST, "The current password isn't right."));

    let (status, headers, _) = s.call("POST", "/api/password", me, Some(change("correct horse", "new pass"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let fresh = headers.iter().find(|(k, _)| k == "set-cookie").unwrap().1.split(';').next().unwrap().to_owned();

    // Every old session is signed out; the one that changed it gets a new cookie.
    assert_eq!(s.call("GET", "/api/overview", Some(&other_session), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(s.call("GET", "/api/overview", me, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(s.call("GET", "/api/overview", Some(&fresh), None).await.0, StatusCode::OK);

    // The old password stops working and the new one works, overriding the configured hash.
    assert_eq!(s.call("POST", "/api/login", None, Some(json!({"password": "correct horse"}))).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(s.call("POST", "/api/login", None, Some(json!({"password": "new pass"}))).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn computers_report_accounts_and_parents_assign_them() {
    let s = server(with_password()).await;
    let pc1 = db::add_client(&s.state.db, "Ryan-PC").await.unwrap();
    let report = json!({"accounts": [
        {"name": "ryany", "admin": false, "configured_kid": "ryan"},
        {"name": "chloe", "admin": false},
        {"name": "benbo", "admin": true, "configured_kid": "ryan"},
    ]});
    // What the install set up carries over; admins are never limited.
    let (status, _, body) = s.call("POST", "/api/client/accounts", Some(&pc1), Some(report.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["limited"], json!([{"account": "ryany", "kid": "ryan"}]));

    // The parent sees the accounts, with the unassigned one flagged as new.
    let cookie = s.admin_cookie().await;
    let me = Some(cookie.as_str());
    let (_, _, body) = s.call("GET", "/api/overview", me, None).await;
    let overview: Value = serde_json::from_str(&body).unwrap();
    let accounts = &overview["clients"][0]["accounts"];
    assert_eq!(accounts.as_array().unwrap().len(), 3);
    let chloe = accounts.as_array().unwrap().iter().find(|a| a["name"] == "chloe").unwrap();
    assert_eq!((chloe["kid"].clone(), chloe["new"].clone()), (Value::Null, json!(true)));

    // Assign chloe, rename the kid, and the computer gets the new mapping on its next report.
    assert_eq!(s.call("POST", "/api/users", me, Some(json!({"username": "chloe"}))).await.0, StatusCode::CREATED);
    let (status, _, _) = s.call("PUT", "/api/clients/Ryan-PC/accounts/chloe", me, Some(json!({"kid": "chloe"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(s.call("PUT", "/api/clients/Ryan-PC/accounts/benbo", me, Some(json!({"kid": "chloe"}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(s.call("PUT", "/api/users/ryan/name", me, Some(json!({"name": "Ryan"}))).await.0, StatusCode::NO_CONTENT);
    let (_, _, body) = s.call("POST", "/api/client/accounts", Some(&pc1), Some(report.clone())).await;
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["limited"], json!([{"account": "chloe", "kid": "chloe"}, {"account": "ryany", "kid": "ryan"}]));

    // "Not limited" sticks even though the computer still says ryany was set up for ryan.
    s.call("PUT", "/api/clients/Ryan-PC/accounts/ryany", me, Some(json!({"kid": null}))).await;
    let (_, _, body) = s.call("POST", "/api/client/accounts", Some(&pc1), Some(report)).await;
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["limited"], json!([{"account": "chloe", "kid": "chloe"}]));
    let (_, _, body) = s.call("GET", "/api/overview", me, None).await;
    let overview: Value = serde_json::from_str(&body).unwrap();
    let ryan = overview["users"].as_array().unwrap().iter().find(|u| u["username"] == "ryan").unwrap();
    assert_eq!(ryan["name"], "Ryan");
}

#[tokio::test]
async fn parents_can_lock_and_take_time_away() {
    let s = server(with_password()).await;
    let pc1 = db::add_client(&s.state.db, "Ryan-PC").await.unwrap();
    let pc2 = db::add_client(&s.state.db, "Chloe-PC").await.unwrap();
    let cookie = s.admin_cookie().await;
    let me = Some(cookie.as_str());
    s.call("POST", "/api/users", me, Some(json!({"username": "ryan"}))).await;
    s.call("PUT", "/api/users/ryan/allowances", me, Some(json!([60, 60, 60, 60, 60, 60, 60]))).await;

    // Take 45 minutes away from a 60-minute day, then 30 more: it bottoms out at nothing.
    assert_eq!(s.call("POST", "/api/users/ryan/bonus", me, Some(json!({"minutes": -45}))).await.0, StatusCode::NO_CONTENT);
    let p = s.policy(&pc1, "ryan").await;
    assert_eq!((p.daily_allowance_minutes, p.bonus_minutes), (Some(15), 0));
    s.call("POST", "/api/users/ryan/bonus", me, Some(json!({"minutes": -30}))).await;
    assert_eq!(s.policy(&pc1, "ryan").await.daily_allowance_minutes, Some(0));
    assert_eq!(s.call("POST", "/api/users/ryan/bonus", me, Some(json!({"minutes": -2000}))).await.0, StatusCode::BAD_REQUEST);

    // Lock one computer, then all of them; unlocking all clears both.
    let lock = |computer: Option<&str>, locked: bool| json!({"computer": computer, "locked": locked});
    assert_eq!(s.call("PUT", "/api/users/ryan/lock", me, Some(lock(Some("Ryan-PC"), true))).await.0, StatusCode::NO_CONTENT);
    assert!(s.policy(&pc1, "ryan").await.locked);
    assert!(!s.policy(&pc2, "ryan").await.locked);
    assert_eq!(s.call("PUT", "/api/users/ryan/lock", me, Some(lock(Some("Nope-PC"), true))).await.0, StatusCode::NOT_FOUND);
    s.call("PUT", "/api/users/ryan/lock", me, Some(lock(None, true))).await;
    assert!(s.policy(&pc2, "ryan").await.locked);
    let (_, _, body) = s.call("GET", "/api/overview", me, None).await;
    let overview: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(overview["users"][0]["locks"], json!(["*", "Ryan-PC"]));
    assert_eq!(overview["users"][0]["decision"], json!({"decision": "denied", "reason": {"kind": "locked"}}));
    s.call("PUT", "/api/users/ryan/lock", me, Some(lock(None, false))).await;
    assert!(!s.policy(&pc1, "ryan").await.locked && !s.policy(&pc2, "ryan").await.locked);
}

#[tokio::test]
async fn the_web_password_approves_time_at_the_computers() {
    let s = server(with_password()).await;
    let pc1 = db::add_client(&s.state.db, "Ryan-PC").await.unwrap();
    let me = s.admin_cookie().await;
    let report = json!({"accounts": [{"name": "ryany", "admin": false}]});
    let hash = |body: String| serde_json::from_str::<Value>(&body).unwrap()["parent_pin_hash"].as_str().unwrap().to_owned();

    let first = hash(s.call("POST", "/api/client/accounts", Some(&pc1), Some(report.clone())).await.2);
    assert!(auth::verify_password(&first, "correct horse"));
    // Changing the password changes what the computers accept.
    let change = json!({"current": "correct horse", "new": "battery staple"});
    let (status, headers, _) = s.call("POST", "/api/password", Some(me.as_str()), Some(change)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{headers:?}");
    let second = hash(s.call("POST", "/api/client/accounts", Some(&pc1), Some(report)).await.2);
    assert!(auth::verify_password(&second, "battery staple") && !auth::verify_password(&second, "correct horse"));
}

#[tokio::test]
async fn history_has_daily_totals_and_hours() {
    let s = server(with_password()).await;
    let me = s.admin_cookie().await;
    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let pc2 = db::add_client(&s.state.db, "pc2").await.unwrap();
    let today = Local::now().date_naive();
    for (pc, minutes) in [(&pc1, 10), (&pc1, 25), (&pc2, 5)] {
        let report = UsageReport { username: "kid".into(), host: "x".into(), day: today, active_minutes: minutes, local_bonus_minutes: 0 };
        s.call("POST", "/api/client/usage", Some(pc), Some(serde_json::to_value(&report).unwrap())).await;
    }
    let body: Value = serde_json::from_str(&s.call("GET", "/api/history?days=7", Some(me.as_str()), None).await.2).unwrap();
    let days = body[0]["days"].as_array().unwrap();
    assert_eq!(days.len(), 7);
    let last = &days[6];
    assert_eq!(last["day"], today.to_string());
    assert_eq!(last["minutes"], 30);
    let hour = chrono::Timelike::hour(&Local::now()) as usize;
    assert_eq!(last["hours"][hour], 30);
    assert_eq!(days[0]["minutes"], 0);
    // pc1's two reports are one continuing session; pc2's is its own.
    let sessions = last["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    let pc1_session = sessions.iter().find(|s| s["host"] == "pc1").unwrap();
    assert_eq!(pc1_session["minutes"], 25);
}

#[tokio::test]
async fn web_ui_revalidates_by_content_not_date() {
    let web = tempfile::tempdir().unwrap();
    std::fs::write(web.path().join("index.html"), "<html>v1</html>").unwrap();
    let s = server(Config { web_dir: Some(web.path().into()), ..Config::default() }).await;
    let get = |headers: Vec<(header::HeaderName, String)>| {
        let mut req = Request::get("/index.html");
        for (k, v) in headers {
            req = req.header(k, v);
        }
        s.app.clone().oneshot(req.body(Body::empty()).unwrap())
    };
    let first = get(vec![]).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert!(first.headers().get(header::LAST_MODIFIED).is_none());
    let etag = first.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(get(vec![(header::IF_NONE_MATCH, etag.clone())]).await.unwrap().status(), StatusCode::NOT_MODIFIED);
    // A browser holding a date from before still gets the file.
    let since = (header::IF_MODIFIED_SINCE, "Thu, 01 Jan 2099 00:00:00 GMT".to_owned());
    assert_eq!(get(vec![since]).await.unwrap().status(), StatusCode::OK);

    // A new build is a new tag.
    std::fs::write(web.path().join("index.html"), "<html>v2</html>").unwrap();
    let s2 = server(Config { web_dir: Some(web.path().into()), ..Config::default() }).await;
    let req = Request::get("/").header(header::IF_NONE_MATCH, &etag).body(Body::empty()).unwrap();
    assert_eq!(s2.app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn sessions_end_after_five_idle_minutes() {
    let s = server(with_password()).await;
    let cookie = s.admin_cookie().await;
    let expires = || async {
        sqlx::query_scalar::<_, String>("SELECT expires_at FROM sessions").fetch_one(&s.state.db).await.unwrap()
    };
    let in_minutes = |m: i64| (chrono::Utc::now() + chrono::TimeDelta::minutes(m)).to_rfc3339();
    let db = s.state.db.clone();
    let backdate = |at: String| {
        let db = db.clone();
        async move {
            sqlx::query("UPDATE sessions SET expires_at = ?").bind(at).execute(&db).await.unwrap();
        }
    };
    let background = || {
        let req = Request::get("/api/overview")
            .header(header::COOKIE, &cookie)
            .header("X-Screentime-Background", "1")
            .body(Body::empty())
            .unwrap();
        s.app.clone().oneshot(req)
    };

    assert!(expires().await <= in_minutes(5));
    // The page's own refresh works but doesn't count as use...
    backdate(in_minutes(1)).await;
    assert_eq!(background().await.unwrap().status(), StatusCode::OK);
    assert!(expires().await <= in_minutes(1));
    // ...while anything the parent does starts the five minutes again.
    assert_eq!(s.call("POST", "/api/keepalive", Some(&cookie), None).await.0, StatusCode::NO_CONTENT);
    assert!(expires().await > in_minutes(4));
    // Once they're up, the session is over.
    backdate(in_minutes(-1)).await;
    assert_eq!(s.call("POST", "/api/keepalive", Some(&cookie), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(background().await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn taking_time_away_comes_off_time_approved_at_the_computer() {
    // 120 minutes approved at pc1 with the PIN, 30 used; the parent takes 100 away on the web.
    let s = server(Config::default()).await;
    let pc1 = db::add_client(&s.state.db, "pc1").await.unwrap();
    let pc2 = db::add_client(&s.state.db, "pc2").await.unwrap();
    let today = Local::now().date_naive();
    let uid = db::ensure_user(&s.state.db, "kid", 0).await.unwrap();
    let report = UsageReport { username: "kid".into(), host: "x".into(), day: today, active_minutes: 30, local_bonus_minutes: 120 };
    s.call("POST", "/api/client/usage", Some(&pc1), Some(serde_json::to_value(&report).unwrap())).await;
    db::add_bonus(&s.state.db, uid, today, -100, "web").await.unwrap();

    let p1 = s.policy(&pc1, "kid").await;
    assert_eq!((p1.daily_allowance_minutes, p1.bonus_minutes, p1.taken_minutes), (Some(0), 0, 100));
    // pc1: 120 approved there - 100 taken - 30 used = nothing left.
    let p2 = s.policy(&pc2, "kid").await;
    assert_eq!(p2.daily_allowance_minutes.unwrap() + p2.bonus_minutes, 0);
}

#[tokio::test]
async fn time_left_stops_at_zero() {
    let s = server(Config { default_allowance_minutes: 0, ..with_password() }).await;
    let me = s.admin_cookie().await;
    let today = Local::now().date_naive();
    let uid = db::ensure_user(&s.state.db, "kid", 0).await.unwrap();
    let adjust = |m: i64| s.call("POST", "/api/users/kid/bonus", Some(me.as_str()), Some(json!({ "minutes": m })));
    adjust(30).await;
    adjust(-120).await;
    assert_eq!(db::time_left(&s.state.db, uid, today).await.unwrap(), Some(0));
    // Time added after taking away "too much" counts in full.
    adjust(15).await;
    assert_eq!(db::time_left(&s.state.db, uid, today).await.unwrap(), Some(15));
    let overview: Value = serde_json::from_str(&s.call("GET", "/api/overview", Some(me.as_str()), None).await.2).unwrap();
    assert_eq!(overview["users"][0]["time_left"], 15);
}

#[tokio::test]
async fn a_new_request_replaces_the_kids_older_one() {
    let s = server(with_password()).await;
    let me = s.admin_cookie().await;
    let kid = db::ensure_user(&s.state.db, "kid", 0).await.unwrap();
    let other = db::ensure_user(&s.state.db, "other", 0).await.unwrap();
    let (old, old_token) = db::create_request(&s.state.db, kid, "pc1", None, None).await.unwrap();
    db::create_request(&s.state.db, other, "pc1", None, None).await.unwrap();
    let (new, _) = db::create_request(&s.state.db, kid, "pc2", None, None).await.unwrap();

    let overview: Value = serde_json::from_str(&s.call("GET", "/api/overview", Some(me.as_str()), None).await.2).unwrap();
    let ids: Vec<i64> = overview["pending_requests"].as_array().unwrap().iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert!(ids.contains(&new.id) && !ids.contains(&old.id) && ids.len() == 2, "{ids:?}");
    // The replaced one's notification buttons no longer add time.
    assert_eq!(s.call("POST", &format!("/act/{}/{old_token}/30", old.id), None, None).await.0, StatusCode::CONFLICT);
    assert_eq!(db::bonus(&s.state.db, kid, Local::now().date_naive()).await.unwrap(), 0);
    let tag = screentime_server::push::request_payload(&new, String::new(), &[]).tag;
    assert_eq!(tag.as_deref(), Some("request-kid"));
}

/// A stand-in for the Netgear router's web admin: one password, a session cookie, the Advanced
/// Wireless page with its one-time token, and the form post that switches the radios.
#[derive(Default)]
struct FakeRouter {
    /// wl_enable_router of each accepted change, in order.
    changes: Vec<String>,
    logins: u32,
    logouts: u32,
    /// What the radios are set to now. The real router starts with them on.
    radios_on: bool,
}

async fn fake_router(password: &'static str) -> (String, Arc<Mutex<FakeRouter>>) {
    use axum::extract::Request as AxRequest;
    use base64ct::{Base64, Encoding};
    let seen = Arc::new(Mutex::new(FakeRouter { radios_on: true, ..FakeRouter::default() }));
    let state = seen.clone();
    let handler = move |req: AxRequest| {
        let seen = state.clone();
        async move {
            let path = req.uri().path().to_owned();
            let query = req.uri().query().unwrap_or("").to_owned();
            let cookie = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
            let browser = req.headers().get(header::USER_AGENT).is_some_and(|v| v.as_bytes().starts_with(b"Mozilla/"));
            let signed_in = cookie.contains("sess=good") && browser;
            let body = String::from_utf8(req.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
            let form: Vec<(String, String)> = serde_urlencoded::from_str(&body).unwrap_or_default();
            let field = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let ok = |extra: Option<&str>, html: String| {
                let mut r = axum::response::Response::builder().status(200);
                if let Some(c) = extra {
                    r = r.header(header::SET_COOKIE, c);
                }
                r.body(Body::from(html)).unwrap()
            };
            match (path.as_str(), query.as_str()) {
                ("/apply.cgi", "%20timestamp=undefined") => {
                    seen.lock().unwrap().logins += 1;
                    let outer = String::from_utf8(Base64::decode_vec(&field("password")).unwrap()).unwrap();
                    let inner = Base64::decode_vec(&outer[1..outer.len() - 1]).unwrap();
                    let good = field("submit_flag") == "admin_login" && field("username") == "admin" && inner == password.as_bytes();
                    ok(good.then_some("sess=good;Path=/;HttpOnly;SameSite=Lax;"), String::new())
                }
                ("/WLG_adv.htm", _) if signed_in => {
                    let on = if seen.lock().unwrap().radios_on { "1" } else { "0" };
                    let page = format!(
                        "var con_endis_wl_radio='{on}';\nvar con_endis_wla_radio='{on}';\n\
                         <form action=\"/apply.cgi?/WLG_adv.htm timestamp=99887766\" method=\"post\">"
                    );
                    ok(None, page)
                }
                ("/WLG_adv.htm", _) => ok(
                    None,
                    r#"else if(result == 3) top.location.href="multi_login.html"; <input type="password" name="password">"#.into(),
                ),
                ("/apply.cgi", "/WLG_adv.htm%20timestamp=99887766") if signed_in => {
                    assert_eq!(field("wla_enable_router"), field("wl_enable_router"));
                    assert_eq!(field("wla_6g_enable_router"), "1");
                    let mut r = seen.lock().unwrap();
                    r.radios_on = field("wl_enable_router") == "1";
                    r.changes.push(field("wl_enable_router"));
                    ok(None, "<html>applied</html>".into())
                }
                ("/LGO_logout.htm", _) => {
                    seen.lock().unwrap().logouts += 1;
                    ok(None, String::new())
                }
                _ => axum::response::Response::builder().status(400).body(Body::empty()).unwrap(),
            }
        }
    };
    let app = Router::new().fallback(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn wait_for(mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("timed out");
}

#[tokio::test]
async fn wifi_is_a_built_in_user_that_switches_the_router() {
    let s = server(with_password()).await;
    let me = s.admin_cookie().await;
    let me = Some(me.as_str());
    let (url, router) = fake_router("router secret").await;
    tokio::spawn(screentime_server::wifi::run(s.state.clone()));

    // Wi-Fi is there from the start, after the kids, with no limit; nothing touches the router.
    s.call("POST", "/api/users", me, Some(json!({"username": "ryan"}))).await;
    let o: Value = serde_json::from_str(&s.call("GET", "/api/overview", me, None).await.2).unwrap();
    let users = o["users"].as_array().unwrap();
    assert_eq!(users.last().unwrap()["username"], "wifi");
    assert_eq!(users.last().unwrap()["name"], "Wi-Fi");
    assert_eq!(users.last().unwrap()["wifi"], true);
    assert_eq!(users[0]["wifi"], false);
    assert_eq!(o["wifi"]["enabled"], false);
    assert_eq!(o["wifi"]["wanted_on"], Value::Null);
    // It's not a kid: computers can't ask for it, accounts can't be it, and it can't be re-added.
    let pc = db::add_client(&s.state.db, "pc1").await.unwrap();
    assert_eq!(s.call("GET", "/api/client/policy/wifi", Some(&pc), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(s.call("POST", "/api/users", me, Some(json!({"username": "wifi"}))).await.0, StatusCode::BAD_REQUEST);
    let h: Value = serde_json::from_str(&s.call("GET", "/api/history?days=3", me, None).await.2).unwrap();
    assert!(h.as_array().unwrap().iter().any(|k| k["username"] == "wifi" && k["wifi"] == true));

    assert_eq!(s.call("PUT", "/api/wifi/settings", me, Some(json!({"enabled": true}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(s.call("PUT", "/api/wifi/settings", me, Some(json!({"router_url": "ftp://x"}))).await.0, StatusCode::BAD_REQUEST);
    let settings = json!({"router_url": url, "router_user": "admin", "router_password": "router secret"});
    let (status, _, body) = s.call("PUT", "/api/wifi/settings", me, Some(settings)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("router secret"), "the password never goes back to the browser");
    assert!(!s.call("GET", "/api/overview", me, None).await.2.contains("router secret"));
    assert!(router.lock().unwrap().logins == 0);

    // A zero-minute day: turning Wi-Fi time on switches the Wi-Fi off.
    s.call("PUT", "/api/users/wifi/allowances", me, Some(json!([0, 0, 0, 0, 0, 0, 0]))).await;
    s.call("PUT", "/api/wifi/settings", me, Some(json!({"enabled": true}))).await;
    wait_for(|| router.lock().unwrap().changes == ["0"]).await;
    // Adding time turns it on; taking it away turns it off again.
    s.call("POST", "/api/users/wifi/bonus", me, Some(json!({"minutes": 30}))).await;
    wait_for(|| router.lock().unwrap().changes == ["0", "1"]).await;
    let o: Value = serde_json::from_str(&s.call("GET", "/api/overview", me, None).await.2).unwrap();
    assert_eq!(o["wifi"]["router"]["on"], true);
    assert_eq!(o["users"].as_array().unwrap().last().unwrap()["time_left"], 30);
    s.call("POST", "/api/users/wifi/bonus", me, Some(json!({"minutes": -45}))).await;
    wait_for(|| router.lock().unwrap().changes == ["0", "1", "0"]).await;
    // A lock turns it off whatever the time; unlocking with time left turns it on.
    s.call("POST", "/api/users/wifi/bonus", me, Some(json!({"minutes": 15}))).await;
    wait_for(|| router.lock().unwrap().changes.len() == 4).await;
    s.call("PUT", "/api/users/wifi/lock", me, Some(json!({"computer": null, "locked": true}))).await;
    wait_for(|| router.lock().unwrap().changes == ["0", "1", "0", "1", "0"]).await;
    s.call("PUT", "/api/users/wifi/lock", me, Some(json!({"computer": null, "locked": false}))).await;
    wait_for(|| router.lock().unwrap().changes.len() == 6).await;
    // Downtime all day turns it off.
    let all_day = json!([{"days": ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"], "start": "00:00:00", "end": "00:00:00"}]);
    let (status, _, body) = s.call("PUT", "/api/users/wifi/downtime", me, Some(all_day)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    wait_for(|| router.lock().unwrap().changes.len() == 7).await;
    assert_eq!(router.lock().unwrap().changes.last().unwrap(), "0");
    s.call("PUT", "/api/users/wifi/downtime", me, Some(json!([]))).await;
    wait_for(|| router.lock().unwrap().changes.len() == 8).await;
    // Every login was followed by a logout, and there were no errors.
    wait_for(|| { let r = router.lock().unwrap(); r.logins == r.logouts }).await;
    let w: Value = serde_json::from_str(&s.call("GET", "/api/wifi", me, None).await.2).unwrap();
    assert_eq!(w["router"]["last_error"], Value::Null);
    // Turning Wi-Fi time off while the server had the Wi-Fi off turns it back on.
    s.call("PUT", "/api/users/wifi/lock", me, Some(json!({"computer": null, "locked": true}))).await;
    wait_for(|| router.lock().unwrap().changes.len() == 9).await;
    s.call("PUT", "/api/wifi/settings", me, Some(json!({"enabled": false}))).await;
    wait_for(|| router.lock().unwrap().changes.len() == 10).await;
    assert_eq!(router.lock().unwrap().changes.last().unwrap(), "1");
}

#[tokio::test]
async fn wifi_minutes_count_while_it_is_on() {
    let s = server(with_password()).await;
    let id = db::user_id(&s.state.db, "wifi").await.unwrap().unwrap();
    let today = Local::now().date_naive();
    // The server records them as use on the "router", like a computer reporting a kid's use.
    let report = UsageReport { username: "wifi".into(), host: "router".into(), day: today, active_minutes: 5, local_bonus_minutes: 0 };
    db::record_usage(&s.state.db, id, "router", &report).await.unwrap();
    db::add_bonus(&s.state.db, id, today, 10, "web").await.unwrap();
    db::set_allowances(&s.state.db, id, &[Some(0); 7]).await.unwrap();
    assert_eq!(db::time_left(&s.state.db, id, today).await.unwrap(), Some(5));
    let h = db::history(&s.state.db, id, today, 1).await.unwrap();
    assert_eq!(h[0].minutes, 5);
}

#[tokio::test]
async fn wifi_reports_a_refused_router_login() {
    let s = server(with_password()).await;
    let me = s.admin_cookie().await;
    let me = Some(me.as_str());
    let (url, router) = fake_router("router secret").await;
    tokio::spawn(screentime_server::wifi::run(s.state.clone()));
    let settings = json!({"router_url": url, "router_password": "wrong", "enabled": true});
    assert_eq!(s.call("PUT", "/api/wifi/settings", me, Some(settings)).await.0, StatusCode::OK);
    let mut error = Value::Null;
    for _ in 0..200 {
        let w: Value = serde_json::from_str(&s.call("GET", "/api/wifi", me, None).await.2).unwrap();
        error = w["router"]["last_error"].clone();
        if !error.is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(error.as_str().unwrap().contains("refused the login"), "{error}");
    // A refused login isn't retried right away (the router locks the admin out).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let r = router.lock().unwrap();
    assert_eq!((r.logins, r.logouts, r.changes.len()), (1, 1, 0));
}

#[tokio::test]
async fn wifi_already_on_is_left_alone() {
    let s = server(with_password()).await;
    let me = s.admin_cookie().await;
    let me = Some(me.as_str());
    let (url, router) = fake_router("router secret").await;
    tokio::spawn(screentime_server::wifi::run(s.state.clone()));
    // No limit and the radios already on: the server checks, and doesn't re-apply (which would
    // restart the radios).
    let settings = json!({"router_url": url, "router_password": "router secret", "enabled": true});
    assert_eq!(s.call("PUT", "/api/wifi/settings", me, Some(settings)).await.0, StatusCode::OK);
    let mut on = Value::Null;
    for _ in 0..200 {
        let w: Value = serde_json::from_str(&s.call("GET", "/api/wifi", me, None).await.2).unwrap();
        on = w["router"]["on"].clone();
        if !on.is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(on, true);
    let r = router.lock().unwrap();
    assert_eq!((r.logins, r.logouts, r.changes.len()), (1, 1, 0));
}

#[tokio::test]
async fn router_scripts_can_be_edited_and_tested() {
    let s = server(with_password()).await;
    let me = s.admin_cookie().await;
    let me = Some(me.as_str());
    let (url, router) = fake_router("router secret").await;
    let settings = json!({"router_url": url, "router_password": "router secret"});
    assert_eq!(s.call("PUT", "/api/wifi/settings", me, Some(settings)).await.0, StatusCode::OK);

    // The built-in scripts, with the defaults alongside; the password isn't in them.
    let (status, _, body) = s.call("GET", "/api/wifi/scripts", me, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("router secret"));
    let scripts: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(scripts["on"], scripts["default_on"]);
    assert!(scripts["off"].as_str().unwrap().contains("const WANT = \"off\""));

    // Testing the status script reads the radios (on) without changing anything.
    let (_, _, body) = s.call("POST", "/api/wifi/test", me, Some(json!({"script": "status"}))).await;
    let o: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((o["ok"].as_bool(), o["value"].as_bool()), (Some(true), Some(true)), "{o}");
    assert!(router.lock().unwrap().changes.is_empty());
    let w: Value = serde_json::from_str(&s.call("GET", "/api/wifi", me, None).await.2).unwrap();
    assert_eq!(w["router"]["on"], true);

    // Unsaved edits can be tried; logs and errors come back.
    let code = "console.log('user is', router.user); throw new Error('custom failure')";
    let (_, _, body) = s.call("POST", "/api/wifi/test", me, Some(json!({"script": "off", "code": code}))).await;
    let o: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(o["ok"], false);
    assert_eq!(o["message"], "custom failure");
    assert_eq!(o["logs"], json!(["user is admin"]));

    // A saved custom script is what the server runs, and its error shows on the Wi-Fi card.
    let custom = json!({"script_off": "throw new Error('my off script broke')"});
    assert_eq!(s.call("PUT", "/api/wifi/settings", me, Some(custom)).await.0, StatusCode::OK);
    let w: Value = serde_json::from_str(&s.call("GET", "/api/wifi", me, None).await.2).unwrap();
    assert_eq!(w["custom_scripts"], json!(["off"]));
    tokio::spawn(screentime_server::wifi::run(s.state.clone()));
    s.call("PUT", "/api/users/wifi/lock", me, Some(json!({"computer": null, "locked": true}))).await;
    s.call("PUT", "/api/wifi/settings", me, Some(json!({"enabled": true}))).await;
    let mut error = Value::Null;
    for _ in 0..200 {
        let w: Value = serde_json::from_str(&s.call("GET", "/api/wifi", me, None).await.2).unwrap();
        error = w["router"]["last_error"].clone();
        if !error.is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(error, "my off script broke");
    assert!(router.lock().unwrap().changes.is_empty());

    // Saving it empty goes back to the built-in script.
    s.call("PUT", "/api/wifi/settings", me, Some(json!({"script_off": ""}))).await;
    let w: Value = serde_json::from_str(&s.call("GET", "/api/wifi", me, None).await.2).unwrap();
    assert_eq!(w["custom_scripts"], json!([]));
    let too_long = "x".repeat(70_000);
    assert_eq!(s.call("PUT", "/api/wifi/settings", me, Some(json!({"script_on": too_long}))).await.0, StatusCode::BAD_REQUEST);
}
