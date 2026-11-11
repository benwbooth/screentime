//! Router scripts: small JavaScript programs, edited in the web UI, that turn the Wi-Fi on or
//! off or report whether it's on. They run in an embedded QuickJS with no file system or
//! network access beyond an `http()` helper, a time limit and a memory limit.
//!
//! What a script sees:
//! - `router.url`, `router.user`, `router.password`: the router settings (the password comes
//!   from the server's database at run time and never appears in the script text);
//! - `await http({method, url, headers, form, body, timeout})` → `{status, headers, body}`.
//!   A `url` starting with `/` is relative to `router.url`. Cookies persist for the whole run
//!   and a browser User-Agent is sent unless the script sets its own. `form` is an object of
//!   fields, sent URL-encoded in the order given. `timeout` is in seconds (default 10).
//!   Header names in the reply are lower case.
//! - `btoa()`, `atob()`, `await sleep(ms)`, `console.log()` (to the server log and the Test
//!   output), and `RouterError(message, badPassword)`.
//!
//! The script body runs inside an async function, so it can use `await` and `return`. Throwing
//! fails the run, and the error's message is shown on the Wi-Fi card. Throw
//! `new RouterError(msg, true)` when the router refused the password, so the server waits
//! longer before trying again (routers lock the admin out after repeated tries).

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Longest a script may run.
pub const TIME_LIMIT: Duration = Duration::from_secs(60);
const MEMORY_LIMIT: usize = 64 << 20;
pub const MAX_SCRIPT_BYTES: usize = 64 * 1024;
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

pub const DEFAULT_ON: &str = include_str!("router_scripts/on.js");
pub const DEFAULT_OFF: &str = include_str!("router_scripts/off.js");
pub const DEFAULT_STATUS: &str = include_str!("router_scripts/status.js");

#[derive(Debug, Clone, Serialize)]
pub struct RouterConfig {
    pub url: String,
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Outcome {
    pub ok: bool,
    /// What the script returned (for the status script: true, false or null).
    pub value: serde_json::Value,
    /// Why it failed.
    pub message: Option<String>,
    /// The router refused the password.
    pub bad_password: bool,
    /// Its console.log lines.
    pub logs: Vec<String>,
}

impl Outcome {
    fn failed(message: impl Into<String>, logs: Vec<String>) -> Outcome {
        Outcome { ok: false, message: Some(message.into()), logs, ..Outcome::default() }
    }
}

/// Run a script on its own thread (the HTTP helper blocks) and wait for it.
pub async fn run(code: String, router: RouterConfig) -> Outcome {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new().name("router-script".into()).spawn(move || {
        let _ = tx.send(run_blocking(&code, &router, TIME_LIMIT));
    });
    if let Err(e) = spawned {
        return Outcome::failed(format!("couldn't start the script: {e}"), Vec::new());
    }
    rx.await.unwrap_or_else(|_| Outcome::failed("the script crashed", Vec::new()))
}

const PRELUDE: &str = r#"
globalThis.router = Object.freeze(JSON.parse(__config));
delete globalThis.__config;
globalThis.http = async (req) => {
  req = Object.assign({}, req);
  if (req.form && !Array.isArray(req.form)) {
    req.form = Object.entries(req.form).map(([k, v]) => [k, v == null ? "" : String(v)]);
  }
  return JSON.parse(__http(JSON.stringify(req)));
};
globalThis.sleep = async (ms) => __sleep(Number(ms) || 0);
globalThis.btoa = (s) => __btoa(String(s));
globalThis.atob = (s) => __atob(String(s));
const __show = (x) => typeof x === "string" ? x : (x instanceof Error ? String(x) : JSON.stringify(x));
globalThis.console = { log: (...a) => __log(a.map(__show).join(" ")) };
console.info = console.warn = console.error = console.debug = console.log;
globalThis.RouterError = class RouterError extends Error {
  constructor(message, badPassword) { super(message); this.name = "RouterError"; this.badPassword = !!badPassword; }
};
"#;

#[derive(Deserialize)]
struct HttpRequest {
    method: Option<String>,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    form: Option<Vec<(String, String)>>,
    body: Option<String>,
    /// Seconds.
    timeout: Option<f64>,
}

#[derive(Serialize)]
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

fn http(client: &reqwest::blocking::Client, base: &str, deadline: Instant, req: &str) -> Result<String, String> {
    let req: HttpRequest = serde_json::from_str(req).map_err(|e| format!("http(): {e}"))?;
    let url = if req.url.starts_with('/') { format!("{}{}", base.trim_end_matches('/'), req.url) } else { req.url.clone() };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("http(): not an http(s) URL: {url}"));
    }
    let method = reqwest::Method::from_bytes(req.method.as_deref().unwrap_or("GET").to_ascii_uppercase().as_bytes())
        .map_err(|_| "http(): bad method".to_string())?;
    let left = deadline.saturating_duration_since(Instant::now());
    let timeout = Duration::from_secs_f64(req.timeout.unwrap_or(10.0).clamp(0.1, 60.0)).min(left);
    if timeout.is_zero() {
        return Err("the script ran out of time".into());
    }
    let mut builder = client.request(method, &url).timeout(timeout);
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    if let Some(form) = &req.form {
        builder = builder.form(form);
    } else if let Some(body) = req.body {
        builder = builder.body(body);
    }
    let resp = builder.send().map_err(|e| {
        if e.is_timeout() {
            return format!("{url} didn't answer in time");
        }
        // reqwest's own message is just "error sending request"; the cause is further down.
        let mut cause = String::from("error sending request");
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            cause = s.to_string();
            source = s.source();
        }
        format!("couldn't reach {url} ({cause})")
    })?;
    let status = resp.status().as_u16();
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in resp.headers() {
        let v = String::from_utf8_lossy(v.as_bytes()).into_owned();
        headers
            .entry(k.as_str().to_owned())
            .and_modify(|old| {
                old.push_str(if k == reqwest::header::SET_COOKIE { "\n" } else { ", " });
                old.push_str(&v);
            })
            .or_insert(v);
    }
    let body = resp.text().map_err(|e| format!("reading {url}: {e}"))?;
    serde_json::to_string(&HttpResponse { status, headers, body }).map_err(|e| e.to_string())
}

fn latin1_to_base64(s: &str) -> Result<String, String> {
    use base64ct::{Base64, Encoding};
    let bytes: Option<Vec<u8>> = s.chars().map(|c| u8::try_from(c as u32).ok()).collect();
    Ok(Base64::encode_string(&bytes.ok_or("btoa(): characters outside Latin-1")?))
}

fn base64_to_latin1(s: &str) -> Result<String, String> {
    use base64ct::{Base64, Encoding};
    let cleaned: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let bytes = Base64::decode_vec(&cleaned).map_err(|_| "atob(): not base64".to_string())?;
    Ok(bytes.into_iter().map(char::from).collect())
}

pub fn run_blocking(code: &str, router: &RouterConfig, limit: Duration) -> Outcome {
    use rquickjs::{Context, Ctx, Exception, Function, Promise, Runtime};

    let logs = Rc::new(RefCell::new(Vec::<String>::new()));
    let deadline = Instant::now() + limit;
    let fail = |m: String, logs: &Rc<RefCell<Vec<String>>>| Outcome::failed(m, logs.borrow().clone());

    let client = match reqwest::blocking::Client::builder().cookie_store(true).user_agent(USER_AGENT).build() {
        Ok(c) => Rc::new(c),
        Err(e) => return fail(format!("HTTP client: {e}"), &logs),
    };
    let (Ok(rt), base) = (Runtime::new(), router.url.clone()) else {
        return fail("couldn't start JavaScript".into(), &logs);
    };
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(1 << 20);
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));
    let Ok(context) = Context::full(&rt) else {
        return fail("couldn't start JavaScript".into(), &logs);
    };
    let config = serde_json::to_string(router).unwrap_or_default();
    let wrapped = format!(
        "(async () => {{\n{code}\n}})().then(\
           v => JSON.stringify({{ ok: true, value: v === undefined ? null : v }}),\
           e => JSON.stringify({{ ok: false, message: String((e && e.message) || e), badPassword: !!(e && e.badPassword) }}))"
    );

    let result: Result<String, String> = context.with(|ctx| {
        let error = |ctx: &Ctx, e: rquickjs::Error| -> String {
            if let rquickjs::Error::Exception = e {
                let v = ctx.catch();
                if let Some(ex) = v.as_exception() {
                    let msg = ex.message().unwrap_or_default();
                    return if Instant::now() > deadline { "the script ran out of time".into() } else { msg };
                }
                return format!("{v:?}");
            }
            e.to_string()
        };
        let setup = || -> rquickjs::Result<()> {
            let g = ctx.globals();
            g.set("__config", config.as_str())?;
            let (client, base) = (client.clone(), base.clone());
            g.set(
                "__http",
                Function::new(ctx.clone(), move |ctx: Ctx<'_>, req: String| -> rquickjs::Result<String> {
                    http(&client, &base, deadline, &req).map_err(|m| Exception::throw_message(&ctx, &m))
                })?,
            )?;
            g.set(
                "__sleep",
                Function::new(ctx.clone(), move |ms: f64| {
                    let want = Duration::from_millis(ms.clamp(0.0, 60_000.0) as u64);
                    std::thread::sleep(want.min(deadline.saturating_duration_since(Instant::now())));
                })?,
            )?;
            let log_sink = logs.clone();
            g.set(
                "__log",
                Function::new(ctx.clone(), move |line: String| {
                    tracing::info!(target: "router_script", "{line}");
                    let mut l = log_sink.borrow_mut();
                    if l.len() < 200 {
                        l.push(line.chars().take(2000).collect());
                    }
                })?,
            )?;
            g.set(
                "__btoa",
                Function::new(ctx.clone(), |ctx: Ctx<'_>, s: String| -> rquickjs::Result<String> {
                    latin1_to_base64(&s).map_err(|m| Exception::throw_message(&ctx, &m))
                })?,
            )?;
            g.set(
                "__atob",
                Function::new(ctx.clone(), |ctx: Ctx<'_>, s: String| -> rquickjs::Result<String> {
                    base64_to_latin1(&s).map_err(|m| Exception::throw_message(&ctx, &m))
                })?,
            )?;
            ctx.eval::<(), _>(PRELUDE)?;
            Ok(())
        };
        setup().map_err(|e| error(&ctx, e))?;
        let promise: Promise = ctx.eval(wrapped.as_bytes().to_vec()).map_err(|e| error(&ctx, e))?;
        promise.finish::<String>().map_err(|e| match e {
            rquickjs::Error::WouldBlock => "the script never finished".to_string(),
            e => error(&ctx, e),
        })
    });

    let logs_now = logs.borrow().clone();
    match result {
        Err(message) => Outcome::failed(message, logs_now),
        Ok(json) => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Reply {
                ok: bool,
                #[serde(default)]
                value: serde_json::Value,
                message: Option<String>,
                #[serde(default)]
                bad_password: bool,
            }
            match serde_json::from_str::<Reply>(&json) {
                Ok(r) if r.ok => Outcome { ok: true, value: r.value, logs: logs_now, ..Outcome::default() },
                Ok(r) => {
                    // An interrupted run surfaces as a generic error; say what happened.
                    let message = if Instant::now() > deadline {
                        "the script ran out of time".to_string()
                    } else {
                        r.message.unwrap_or_else(|| "the script failed".into())
                    };
                    Outcome { ok: false, message: Some(message), bad_password: r.bad_password, logs: logs_now, ..Outcome::default() }
                }
                Err(e) => Outcome::failed(format!("unexpected script result: {e}"), logs_now),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RouterConfig {
        RouterConfig { url: "http://127.0.0.1:9".into(), user: "admin".into(), password: "pw".into() }
    }

    #[test]
    fn returns_values_and_logs() {
        let o = run_blocking("console.log('hi', 1, {a: 2}); return router.user === 'admin' && btoa('pw') === 'cHc=';", &cfg(), TIME_LIMIT);
        assert!(o.ok, "{o:?}");
        assert_eq!(o.value, serde_json::json!(true));
        assert_eq!(o.logs, vec!["hi 1 {\"a\":2}"]);
        assert_eq!(run_blocking("return atob(btoa('a\u{e9}'))", &cfg(), TIME_LIMIT).value, "a\u{e9}");
        assert_eq!(run_blocking("await sleep(1); return null", &cfg(), TIME_LIMIT).value, serde_json::Value::Null);
    }

    #[test]
    fn errors_come_back_as_messages() {
        let o = run_blocking("throw new Error('nope')", &cfg(), TIME_LIMIT);
        assert_eq!((o.ok, o.message.as_deref(), o.bad_password), (false, Some("nope"), false));
        let o = run_blocking("throw new RouterError('refused', true)", &cfg(), TIME_LIMIT);
        assert_eq!((o.ok, o.message.as_deref(), o.bad_password), (false, Some("refused"), true));
        let o = run_blocking("this is not javascript", &cfg(), TIME_LIMIT);
        assert!(!o.ok && o.message.is_some(), "{o:?}");
        let o = run_blocking("await http({url: '/x', timeout: 2})", &cfg(), TIME_LIMIT);
        assert!(o.message.unwrap().contains("couldn't reach http://127.0.0.1:9/x"));
        let o = run_blocking("await http({url: 'file:///etc/passwd'})", &cfg(), TIME_LIMIT);
        assert!(o.message.unwrap().contains("not an http(s) URL"));
    }

    #[test]
    fn runaway_scripts_are_stopped() {
        let started = Instant::now();
        let o = run_blocking("while (true) {}", &cfg(), Duration::from_millis(300));
        assert_eq!(o.message.as_deref(), Some("the script ran out of time"));
        assert!(started.elapsed() < Duration::from_secs(5));
        let o = run_blocking("let a = []; while (true) a.push('x'.repeat(1000000));", &cfg(), TIME_LIMIT);
        assert!(!o.ok);
    }

    #[test]
    fn default_scripts_compile() {
        for code in [DEFAULT_ON, DEFAULT_OFF, DEFAULT_STATUS] {
            // Point them at a closed port: they must fail on the network, not on syntax.
            let o = run_blocking(code, &cfg(), TIME_LIMIT);
            assert!(o.message.as_deref().unwrap_or("").contains("couldn't reach"), "{o:?}");
        }
    }
}
