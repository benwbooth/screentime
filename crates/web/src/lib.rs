//! The screentime admin UI: a Leptos single-page app talking to the server's JSON API.

mod api;

use screentime_common::{Decision, DenyReason, DowntimeWindow};
use api::{ApiError, Overview, RequestRow, UserOverview};
use chrono::{NaiveTime, Weekday};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/push.js")]
extern "C" {
    #[wasm_bindgen(js_name = pushSupport)]
    fn push_support() -> String;
    #[wasm_bindgen(js_name = pushEnabled)]
    async fn push_enabled() -> JsValue;
    #[wasm_bindgen(js_name = enablePush, catch)]
    async fn enable_push(public_key: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_name = deviceLabel)]
    fn device_label() -> String;
    #[wasm_bindgen(js_name = localTime)]
    fn local_time(iso: &str) -> String;
    #[wasm_bindgen(js_name = localDateTime)]
    fn local_date_time(iso: &str) -> String;
    #[wasm_bindgen(js_name = copyText, catch)]
    async fn copy_text(text: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_name = confirmAction)]
    fn confirm_action(message: &str) -> bool;
}

const WEEKDAYS: [Weekday; 7] =
    [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat, Weekday::Sun];
const PRESETS: [u32; 4] = [15, 30, 45, 60];

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    Checking,
    SignedOut,
    SignedIn,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Today,
    History,
    Schedules,
    Machines,
    Notifications,
    Account,
}

impl Tab {
    const ALL: [Tab; 6] = [Tab::Today, Tab::History, Tab::Schedules, Tab::Machines, Tab::Notifications, Tab::Account];

    fn label(self) -> &'static str {
        match self {
            Tab::Today => "Today",
            Tab::History => "History",
            Tab::Schedules => "Schedules",
            Tab::Machines => "Computers",
            Tab::Notifications => "Alerts",
            Tab::Account => "Account",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Tab::Today => "M12 7v5l3 2M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18z",
            Tab::History => "M4 20V11M10 20V5M16 20v-6M2 20h20",
            Tab::Schedules => "M4 6h16M4 12h16M4 18h10M8 3v6M16 3v6",
            Tab::Machines => "M3 5h18v11H3zM8 20h8M12 16v4",
            Tab::Notifications => "M6 16V11a6 6 0 1 1 12 0v5l2 2H4zM10 20a2 2 0 0 0 4 0",
            Tab::Account => "M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM4 21a8 8 0 0 1 16 0",
        }
    }
}

/// Shared app state, provided as context.
#[derive(Clone, Copy)]
struct Ctx {
    auth: RwSignal<Auth>,
    overview: RwSignal<Option<Overview>>,
    toast: RwSignal<Option<(String, bool)>>,
    tab: RwSignal<Tab>,
}

impl Ctx {
    fn refresh(self) {
        self.load(false);
    }

    fn load(self, background: bool) {
        spawn_local(async move {
            let result = if background { api::overview_in_background().await } else { api::overview().await };
            match result {
                Ok(o) => {
                    self.overview.set(Some(o));
                    self.set_auth(Auth::SignedIn);
                }
                Err(ApiError::Unauthorized) => self.set_auth(Auth::SignedOut),
                Err(e) => {
                    if self.auth.get_untracked() == Auth::Checking {
                        self.set_auth(Auth::SignedOut);
                    }
                    self.say(e.to_string(), false);
                }
            }
        });
    }

    /// Setting a signal re-runs its views even when the value is unchanged, which would rebuild
    /// the whole page (and drop unsaved edits) on every refresh.
    fn set_auth(self, auth: Auth) {
        if self.auth.get_untracked() != auth {
            self.auth.set(auth);
        }
    }

    fn say(self, message: impl Into<String>, ok: bool) {
        self.toast.set(Some((message.into(), ok)));
        let toast = self.toast;
        spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(4000).await;
            toast.set(None);
        });
    }

    /// Run an API call; on success show `done` and reload the overview.
    fn act<F>(self, done: impl Into<String> + 'static, call: F)
    where
        F: std::future::Future<Output = Result<(), ApiError>> + 'static,
    {
        let done = done.into();
        spawn_local(async move {
            match call.await {
                Ok(()) => {
                    if !done.is_empty() {
                        self.say(done, true);
                    }
                    self.refresh();
                }
                Err(ApiError::Unauthorized) => self.set_auth(Auth::SignedOut),
                Err(e) => self.say(e.to_string(), false),
            }
        });
    }
}

fn ctx() -> Ctx {
    expect_context::<Ctx>()
}

fn fmt_minutes(m: u32) -> String {
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

fn js_error(e: JsValue) -> String {
    e.dyn_into::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .unwrap_or_else(|v| v.as_string().unwrap_or_else(|| "Something went wrong.".into()))
}

/// Headline and tone for a kid's current state.
fn status(user: &UserOverview, today: chrono::NaiveDate) -> (String, &'static str) {
    match &user.decision {
        Decision::Allowed { remaining_minutes: Some(m) } => {
            (format!("{} left", fmt_minutes(*m)), if *m <= 10 { "warn" } else { "ok" })
        }
        Decision::Allowed { remaining_minutes: None } => ("No limit right now".into(), "ok"),
        Decision::Denied { reason: DenyReason::Downtime { until } } => {
            let when = if until.date() == today {
                until.format("%-I:%M %p").to_string()
            } else {
                until.format("%a %-I:%M %p").to_string()
            };
            (format!("Downtime until {when}"), "off")
        }
        Decision::Denied { reason: DenyReason::OutOfTime } => ("Time's up for today".into(), "off"),
        Decision::Denied { reason: DenyReason::NoPolicy } => ("Not set up".into(), "off"),
        Decision::Denied { reason: DenyReason::Locked } => ("Locked".into(), "off"),
    }
}

#[component]
fn App() -> impl IntoView {
    let ctx = Ctx {
        auth: RwSignal::new(Auth::Checking),
        overview: RwSignal::new(None),
        toast: RwSignal::new(None),
        tab: RwSignal::new(Tab::Today),
    };
    provide_context(ctx);
    ctx.refresh();

    // Keep the page current while it's open.
    spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(30_000).await;
            if ctx.auth.get_untracked() == Auth::SignedIn {
                ctx.load(true);
            }
        }
    });
    watch_for_idle(ctx);

    view! {
        {move || match ctx.auth.get() {
            Auth::Checking => view! { <div class="splash"><Logo/></div> }.into_any(),
            Auth::SignedOut => view! { <Login/> }.into_any(),
            Auth::SignedIn => view! { <Shell/> }.into_any(),
        }}
        {move || ctx.toast.get().map(|(msg, ok)| view! {
            <div class="toast" class:bad=!ok role="status">{msg}</div>
        })}
    }
}

/// Sign out after IDLE_MS without a tap or key press, so a kid can't pick up a parent's phone or
/// computer and give themselves time. The server enforces the same limit; this keeps it alive
/// while the parent is busy on the page and shows the sign-in screen as soon as time is up.
const IDLE_MS: f64 = 5.0 * 60.0 * 1000.0;
const KEEPALIVE_MS: f64 = 60.0 * 1000.0;

fn watch_for_idle(ctx: Ctx) {
    use std::cell::Cell;
    use std::rc::Rc;
    let last_active = Rc::new(Cell::new(js_sys::Date::now()));
    let last_ping = Rc::new(Cell::new(js_sys::Date::now()));

    let check = {
        let last_active = last_active.clone();
        move || {
            if ctx.auth.get_untracked() == Auth::SignedIn && js_sys::Date::now() - last_active.get() >= IDLE_MS {
                spawn_local(async move {
                    let _ = api::logout().await;
                });
                ctx.overview.set(None);
                ctx.set_auth(Auth::SignedOut);
                ctx.say("Signed out after 5 minutes without use", true);
            }
        }
    };
    let active = {
        let check = check.clone();
        move || {
            // Coming back to a page left open counts as idle first, then as the start of new use.
            check();
            let now = js_sys::Date::now();
            last_active.set(now);
            if ctx.auth.get_untracked() == Auth::SignedIn && now - last_ping.get() >= KEEPALIVE_MS {
                last_ping.set(now);
                spawn_local(async move {
                    if let Err(ApiError::Unauthorized) = api::keepalive().await {
                        ctx.set_auth(Auth::SignedOut);
                    }
                });
            }
        }
    };
    // Leaked on purpose: these listen for the life of the page.
    {
        let active = active.clone();
        std::mem::forget(window_event_listener(leptos::ev::pointerdown, move |_| active()));
    }
    std::mem::forget(window_event_listener(leptos::ev::keydown, move |_| active()));
    {
        let check = check.clone();
        std::mem::forget(window_event_listener(leptos::ev::visibilitychange, move |_| check()));
    }
    spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(15_000).await;
            check();
        }
    });
}

#[component]
fn Logo() -> impl IntoView {
    view! { <img class="logo" src="/icon.svg" alt="" width="32" height="32"/> }
}

#[component]
fn Login() -> impl IntoView {
    let ctx = ctx();
    let password = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::login(&password.get_untracked()).await {
                Ok(()) => {
                    password.set(String::new());
                    ctx.refresh();
                }
                Err(ApiError::Unauthorized) => error.set(Some("That password isn't right.".into())),
                Err(e) => error.set(Some(e.to_string())),
            }
            busy.set(false);
        });
    };
    view! {
        <main class="login">
            <form class="card login-card" on:submit=submit>
                <Logo/>
                <h1>"Screen time"</h1>
                <p class="muted">"Sign in to manage limits and requests."</p>
                <label for="pw">"Password"</label>
                <input id="pw" type="password" autocomplete="current-password" required
                    prop:value=move || password.get()
                    on:input=move |ev| password.set(event_target_value(&ev))/>
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <button class="primary" disabled=move || busy.get()>
                    {move || if busy.get() { "Signing in…" } else { "Sign in" }}
                </button>
            </form>
        </main>
    }
}

#[component]
fn Shell() -> impl IntoView {
    let ctx = ctx();
    let pending = move || ctx.overview.with(|o| o.as_ref().map_or(0, |o| o.pending_requests.len()));
    let new_accounts = move || {
        ctx.overview.with(|o| {
            o.as_ref().map_or(0, |o| o.clients.iter().flat_map(|c| &c.accounts).filter(|a| a.new).count())
        })
    };
    let nav = move || {
        Tab::ALL
            .into_iter()
            .map(|t| {
                view! {
                    <button class="nav-item" class:active=move || ctx.tab.get() == t
                        on:click=move |_| ctx.tab.set(t)>
                        <svg viewBox="0 0 24 24" aria-hidden="true"><path d=t.icon()/></svg>
                        <span>{t.label()}</span>
                        {move || (t == Tab::Today && pending() > 0).then(|| view! { <b class="dot">{pending()}</b> })}
                        {move || (t == Tab::Machines && new_accounts() > 0).then(|| view! { <b class="dot">{new_accounts()}</b> })}
                    </button>
                }
            })
            .collect_view()
    };
    view! {
        <div class="shell">
            <nav class="nav">
                <div class="brand"><Logo/><span>"Screen time"</span></div>
                {nav}
            </nav>
            <main class="content">
                {move || match ctx.tab.get() {
                    Tab::Today => view! { <TodayView/> }.into_any(),
                    Tab::History => view! { <HistoryView/> }.into_any(),
                    Tab::Schedules => view! { <SchedulesView/> }.into_any(),
                    Tab::Machines => view! { <MachinesView/> }.into_any(),
                    Tab::Notifications => view! { <NotificationsView/> }.into_any(),
                    Tab::Account => view! { <AccountView/> }.into_any(),
                }}
            </main>
        </div>
    }
}

/// 15/30/45/60/custom minute buttons, plus "No" when answering a request.
#[component]
fn MinutePicker(
    on_pick: Callback<u32>,
    #[prop(optional)] with_no: bool,
    #[prop(optional)] highlight: Option<u32>,
    /// Show the amounts as time taken away (−15) instead of added.
    #[prop(optional)]
    minus: bool,
) -> impl IntoView {
    let sign = if minus { "\u{2212}" } else { "+" };
    let custom_open = RwSignal::new(false);
    let custom = RwSignal::new(String::new());
    let submit_custom = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if let Ok(m) = custom.get_untracked().trim().parse::<u32>()
            && (1..=1440).contains(&m)
        {
            on_pick.run(m);
            custom.set(String::new());
            custom_open.set(false);
        }
    };
    view! {
        <div class="picker">
            {with_no.then(|| view! { <button class="chip no" on:click=move |_| on_pick.run(0)>"No"</button> })}
            {PRESETS.into_iter().map(|m| view! {
                <button class="chip" class:minus=minus class:suggested=highlight == Some(m) on:click=move |_| on_pick.run(m)>
                    {format!("{sign}{m}")}
                </button>
            }).collect_view()}
            <button class="chip" class:active=move || custom_open.get()
                on:click=move |_| custom_open.update(|o| *o = !*o)>"Custom"</button>
        </div>
        <Show when=move || custom_open.get()>
            <form class="custom" on:submit=submit_custom>
                <input type="number" min="1" max="1440" inputmode="numeric" placeholder="Minutes" required
                    prop:value=move || custom.get() on:input=move |ev| custom.set(event_target_value(&ev))/>
                <button class="primary">{if minus { "Take" } else { "Add" }}</button>
            </form>
        </Show>
    }
}

#[component]
fn TodayView() -> impl IntoView {
    let ctx = ctx();
    let requests = move || ctx.overview.with(|o| o.as_ref().map(|o| o.pending_requests.clone()).unwrap_or_default());
    let users = move || ctx.overview.with(|o| o.as_ref().map(|o| o.users.clone()).unwrap_or_default());
    let today = move || ctx.overview.with(|o| o.as_ref().map(|o| o.today)).unwrap_or_default();
    view! {
        <header class="page-head"><h1>"Today"</h1></header>
        <Show when=move || !requests().is_empty()>
            <section>
                <h2>"Asking for more time"</h2>
                <div class="stack">
                    <For each=requests key=|r| r.id let:r>
                        <RequestCard r=r/>
                    </For>
                </div>
            </section>
        </Show>
        <section>
            <h2>"Kids"</h2>
            <Show when=move || !users().is_empty()
                fallback=|| view! { <p class="empty">"No one yet. Kids appear here once a computer checks in for them, or add one under Schedules."</p> }>
                <div class="grid">
                    <For each=users key=|u| format!("{u:?}") let:u>
                        <KidCard u=u today=today()/>
                    </For>
                </div>
            </Show>
        </section>
    }
}

fn kid_name(ctx: Ctx, username: &str) -> String {
    ctx.overview
        .with_untracked(|o| o.as_ref().and_then(|o| o.users.iter().find(|u| u.username == username).map(|u| u.name.clone())))
        .unwrap_or_else(|| username.to_owned())
}

#[component]
fn RequestCard(r: RequestRow) -> impl IntoView {
    let ctx = ctx();
    let (id, name) = (r.id, kid_name(ctx, &r.username));
    let shown = name.clone();
    let asked = match r.requested_minutes {
        Some(m) => format!("wants {} more", fmt_minutes(m as u32)),
        None => "wants more time".to_owned(),
    };
    let pick = Callback::new(move |m: u32| {
        let done = if m == 0 { format!("Said no to {name}") } else { format!("Gave {name} {m} more minutes") };
        ctx.act(done, api::resolve_request(id, m));
    });
    view! {
        <article class="card request">
            <div class="request-head">
                <div>
                    <strong>{shown}</strong>" "{asked}
                    <div class="muted small">{format!("on {} · {}", r.host, local_time(&r.created_at))}</div>
                </div>
            </div>
            {r.reason.clone().map(|reason| view! { <blockquote>{reason}</blockquote> })}
            <MinutePicker on_pick=pick with_no=true highlight=r.requested_minutes.map(|m| m as u32).unwrap_or(0)/>
        </article>
    }
}

#[component]
fn KidCard(u: UserOverview, today: chrono::NaiveDate) -> impl IntoView {
    let ctx = ctx();
    let (headline, _) = status(&u, today);
    let (username, shown) = (u.username.clone(), u.name.clone());
    let adjust = {
        let (username, shown) = (username.clone(), shown.clone());
        move |minutes: i64| {
            let (username, shown) = (username.clone(), shown.clone());
            let done = if minutes > 0 {
                format!("Added {minutes} minutes for {shown}")
            } else {
                format!("Took time away from {shown}")
            };
            ctx.act(done, async move { api::add_bonus(&username, minutes).await })
        }
    };
    let add = {
        let adjust = adjust.clone();
        Callback::new(move |m: u32| adjust(m as i64))
    };
    let take = Callback::new(move |m: u32| adjust(-(m as i64)));
    // One number: the time the kid has left today. What's stopping them, if anything, under it.
    let left = match u.time_left {
        Some(0) => "No time left".to_string(),
        Some(m) => format!("{} left", fmt_minutes(m)),
        None => "No daily limit".to_string(),
    };
    let left_tone = match u.time_left {
        Some(0) => "off",
        Some(m) if m <= 10 => "warn",
        _ => "ok",
    };
    let blocked = match &u.decision {
        Decision::Denied { reason: DenyReason::OutOfTime } => None,
        Decision::Denied { .. } => Some(headline.clone()),
        Decision::Allowed { .. } => None,
    };
    let details = [
        blocked,
        (!u.locks.is_empty() && !u.locks.iter().any(|l| l == "*")).then(|| format!("Locked on {}", u.locks.join(", "))),
        Some(format!("Used {} today", fmt_minutes(u.used_today))),
    ];
    let locked_all = u.locks.iter().any(|l| l == "*");
    let locked_on = u.locks.clone();
    let computers = move || ctx.overview.with(|o| o.as_ref().map(|o| o.clients.clone()).unwrap_or_default());
    let lock = {
        let (username, shown) = (username.clone(), shown.clone());
        move |computer: Option<String>, locked: bool| {
            let (username, shown) = (username.clone(), shown.clone());
            let where_ = computer.clone().unwrap_or_else(|| "all computers".into());
            let done = if locked { format!("Locked {shown} on {where_}") } else { format!("Unlocked {shown} on {where_}") };
            ctx.act(done, async move { api::set_lock(&username, computer.as_deref(), locked).await })
        }
    };
    let lock_all = lock.clone();
    view! {
        <article class="card kid">
            <div class="kid-head">
                <span class="avatar">{u.name.chars().next().unwrap_or('?').to_uppercase().to_string()}</span>
                <div>
                    <strong>{u.name.clone()}</strong>
                    <div class=format!("status big {left_tone}")>{left}</div>
                </div>
            </div>
            <ul class="facts">{details.into_iter().flatten().map(|d| view! { <li>{d}</li> }).collect_view()}</ul>
            <div class="label">"Add time"</div>
            <MinutePicker on_pick=add/>
            <div class="label spaced">"Take time away"</div>
            <MinutePicker on_pick=take minus=true/>
            <div class="label spaced">"Lock"</div>
            <div class="picker">
                <button class="chip lock" class:active=locked_all
                    on:click=move |_| lock_all(None, !locked_all)>
                    {if locked_all { "Unlock all" } else { "Lock all computers" }}
                </button>
                {move || (!locked_all).then(|| {
                    let lock = lock.clone();
                    let locked_on = locked_on.clone();
                    computers().into_iter().map(move |c| {
                        let on = locked_on.iter().any(|l| l.eq_ignore_ascii_case(&c.hostname));
                        let lock = lock.clone();
                        let host = c.hostname.clone();
                        view! {
                            <button class="chip lock" class:active=on aria-pressed=on.to_string()
                                on:click=move |_| lock(Some(host.clone()), !on)>
                                {format!("{}{}", if on { "\u{1F512} " } else { "" }, c.hostname)}
                            </button>
                        }
                    }).collect_view()
                })}
            </div>
        </article>
    }
}

#[component]
fn HistoryView() -> impl IntoView {
    let ctx = ctx();
    let kids = RwSignal::new(None::<Vec<api::KidHistory>>);
    spawn_local(async move {
        match api::history(28).await {
            Ok(h) => kids.set(Some(h)),
            Err(ApiError::Unauthorized) => ctx.set_auth(Auth::SignedOut),
            Err(e) => ctx.say(e.to_string(), false),
        }
    });
    view! {
        <header class="page-head"><h1>"History"</h1></header>
        {move || match kids.get() {
            None => view! { <p class="muted">"Loading\u{2026}"</p> }.into_any(),
            Some(k) if k.is_empty() => view! { <p class="empty">"No kids yet."</p> }.into_any(),
            Some(k) => view! {
                <div class="grid">{k.into_iter().map(|kid| view! { <KidHistoryCard kid=kid/> }).collect_view()}</div>
            }.into_any(),
        }}
    }
}

/// A kid's last four weeks: weekly totals, the last seven days, and one day hour by hour.
#[component]
fn KidHistoryCard(kid: api::KidHistory) -> impl IntoView {
    use chrono::Datelike;
    let days = kid.days;
    let n = days.len();
    // Calendar weeks, Sunday first, like a wall calendar: (total, days counted so far).
    let today = days.last().map(|d| d.day).unwrap_or_default();
    let this_sunday = today - chrono::Duration::days(today.weekday().num_days_from_sunday() as i64);
    let week = |back: i64| -> (u32, u32) {
        let start = this_sunday - chrono::Duration::days(7 * back);
        let end = (start + chrono::Duration::days(7)).min(today + chrono::Duration::days(1));
        let total = days.iter().filter(|d| d.day >= start && d.day < end).map(|d| d.minutes).sum();
        (total, (end - start).num_days() as u32)
    };
    let weeks: Vec<(u32, u32)> = (0..4).map(week).collect();
    let last7: Vec<api::DayHistory> = days[n.saturating_sub(7)..].to_vec();
    let selected = RwSignal::new(last7.len().saturating_sub(1));
    let day_max = last7.iter().map(|d| d.minutes).max().unwrap_or(0).max(60);
    let week_label = |i: usize| match i {
        0 => "This week".to_string(),
        1 => "Last week".to_string(),
        i => format!("{i} weeks ago"),
    };
    let bars = last7.clone();
    let detail = last7.clone();
    view! {
        <article class="card history">
            <strong>{kid.name}</strong>
            <ul class="weeks">
                {weeks.iter().enumerate().map(|(i, (m, counted))| view! {
                    <li>
                        <span class="muted small">{week_label(i)}</span>
                        <b>{fmt_minutes(*m)}</b>
                        <span class="muted small">{format!("{} a day", fmt_minutes(m / (*counted).max(1)))}</span>
                    </li>
                }).collect_view()}
            </ul>
            <div class="label">"Last 7 days"</div>
            <div class="bars days">
                {bars.into_iter().enumerate().map(|(i, d)| {
                    let height = format!("height: {}%", (d.minutes * 100 / day_max).max(if d.minutes > 0 { 3 } else { 0 }));
                    let title = format!("{}: {}", d.day.format("%a %b %-d"), fmt_minutes(d.minutes));
                    view! {
                        <button class="bar-col" class:sel=move || selected.get() == i title=title.clone()
                            aria-label=title on:click=move |_| selected.set(i)>
                            <span class="bar-value">{if d.minutes > 0 { short_minutes(d.minutes) } else { String::new() }}</span>
                            <span class="bar" style=height></span>
                            <span class="bar-label">{d.day.format("%a").to_string()}</span>
                        </button>
                    }
                }).collect_view()}
            </div>
            {move || {
                let d = detail[selected.get().min(detail.len().saturating_sub(1))].clone();
                let hour_max = d.hours.iter().copied().max().unwrap_or(0).max(15);
                let recorded: u32 = d.hours.iter().sum();
                view! {
                    <div class="label spaced">{format!("{} \u{00b7} {}", d.day.format("%A %b %-d"), fmt_minutes(d.minutes))}</div>
                    {if recorded == 0 && d.minutes > 0 {
                        view! { <p class="muted small">"No hour-by-hour detail for this day: it's only kept from the day it was turned on."</p> }.into_any()
                    } else {
                        view! {
                            <div class="bars hours">
                                {d.hours.iter().enumerate().map(|(h, m)| {
                                    let height = format!("height: {}%", (m * 100 / hour_max).max(if *m > 0 { 4 } else { 0 }));
                                    view! { <span class="bar" style=height title=format!("{}: {}", hour_label(h), fmt_minutes(*m))></span> }
                                }).collect_view()}
                            </div>
                            <div class="hour-axis muted small"><span>"12a"</span><span>"6a"</span><span>"12p"</span><span>"6p"</span><span>"12a"</span></div>
                        }.into_any()
                    }}
                }
            }}
        </article>
    }
}

fn short_minutes(m: u32) -> String {
    if m < 60 { format!("{m}m") } else { format!("{:.1}h", m as f32 / 60.0).replace(".0h", "h") }
}

fn hour_label(h: usize) -> String {
    match h {
        0 => "12 AM".into(),
        12 => "12 PM".into(),
        h if h < 12 => format!("{h} AM"),
        h => format!("{} PM", h - 12),
    }
}

#[component]
fn SchedulesView() -> impl IntoView {
    let ctx = ctx();
    let users = move || ctx.overview.with(|o| o.as_ref().map(|o| o.users.clone()).unwrap_or_default());
    let new_kid = RwSignal::new(String::new());
    let add = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let name = new_kid.get_untracked().trim().to_lowercase();
        if !name.is_empty() {
            new_kid.set(String::new());
            ctx.act(format!("Added {name}"), async move { api::add_user(&name).await });
        }
    };
    view! {
        <header class="page-head"><h1>"Schedules"</h1></header>
        <div class="stack">
            <For each=users key=|u| u.username.clone() let:u>
                <ScheduleEditor u=u/>
            </For>
        </div>
        <form class="card add-row" on:submit=add>
            <label for="newkid">"Add a kid by their Linux user name"</label>
            <div class="row">
                <input id="newkid" placeholder="e.g. sam" autocapitalize="none" autocomplete="off" spellcheck="false"
                    prop:value=move || new_kid.get() on:input=move |ev| new_kid.set(event_target_value(&ev))/>
                <button class="primary">"Add"</button>
            </div>
        </form>
    }
}

fn time_value(t: NaiveTime) -> String {
    t.format("%H:%M").to_string()
}

#[component]
fn ScheduleEditor(u: UserOverview) -> impl IntoView {
    let ctx = ctx();
    let name = u.username.clone();
    let allowances = RwSignal::new(u.allowances);
    let downtime = RwSignal::new(u.downtime.clone());
    let dirty = RwSignal::new(false);

    let save = {
        let name = name.clone();
        move |_| {
            let (name, a, d) = (name.clone(), allowances.get_untracked(), downtime.get_untracked());
            if d.iter().any(|w| w.days.is_empty()) {
                ctx.say("Each downtime needs at least one day.", false);
                return;
            }
            dirty.set(false);
            ctx.act(format!("Saved {name}'s schedule"), async move {
                api::set_allowances(&name, &a).await?;
                api::set_downtime(&name, &d).await
            });
        }
    };

    let limit_rows = WEEKDAYS
        .into_iter()
        .enumerate()
        .map(|(i, day)| {
            let on = move || allowances.with(|a| a[i].is_some());
            let minutes = move || allowances.with(|a| a[i].map(|m| m.to_string()).unwrap_or_default());
            view! {
                <div class="limit-row">
                    <span class="day">{format!("{day}")}</span>
                    <label class="switch">
                        <input type="checkbox" prop:checked=on on:change=move |ev| {
                            let checked = event_target_checked(&ev);
                            allowances.update(|a| a[i] = if checked { Some(120) } else { None });
                            dirty.set(true);
                        }/>
                        <span>{move || if on() { "Limit" } else { "No limit" }}</span>
                    </label>
                    <Show when=on>
                        <input class="minutes" type="number" min="0" max="1440" step="15" inputmode="numeric"
                            prop:value=minutes on:input=move |ev| {
                                if let Ok(m) = event_target_value(&ev).parse::<u32>() {
                                    allowances.update(|a| a[i] = Some(m.min(1440)));
                                    dirty.set(true);
                                }
                            }/>
                        <span class="muted small">{move || allowances.with(|a| a[i].map(fmt_minutes).unwrap_or_default())}</span>
                    </Show>
                </div>
            }
        })
        .collect_view();

    let windows = move || downtime.with(|d| d.iter().cloned().enumerate().collect::<Vec<_>>());

    view! {
        <article class="card schedule">
            <div class="kid-head">
                <span class="avatar">{u.name.chars().next().unwrap_or('?').to_uppercase().to_string()}</span>
                <input class="name-input" aria-label="Name" prop:value=u.name.clone() on:change={
                    let username = name.clone();
                    move |ev| {
                        let (username, shown) = (username.clone(), event_target_value(&ev));
                        ctx.act(format!("Renamed to {shown}"), async move { api::set_name(&username, &shown).await });
                    }
                }/>
            </div>
            <h3>"Daily limit"</h3>
            <div class="limits">{limit_rows}</div>
            <h3>"Downtime"</h3>
            <p class="muted small">"Can't sign in during these times. A window that ends earlier than it starts runs overnight."</p>
            <div class="stack">
                <For each=windows key=|(i, w)| format!("{i}{w:?}") let:item>
                    <DowntimeRow index=item.0 w=item.1 downtime=downtime dirty=dirty/>
                </For>
            </div>
            <button class="ghost" on:click=move |_| {
                downtime.update(|d| d.push(DowntimeWindow {
                    days: vec![Weekday::Sun, Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu],
                    start: NaiveTime::from_hms_opt(21, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
                }));
                dirty.set(true);
            }>"+ Add downtime"</button>
            <div class="save-row">
                <button class="primary" disabled=move || !dirty.get() on:click=save>"Save"</button>
            </div>
        </article>
    }
}

#[component]
fn DowntimeRow(index: usize, w: DowntimeWindow, downtime: RwSignal<Vec<DowntimeWindow>>, dirty: RwSignal<bool>) -> impl IntoView {
    let edit = move |f: Box<dyn FnOnce(&mut DowntimeWindow)>| {
        downtime.update(|d| {
            if let Some(w) = d.get_mut(index) {
                f(w);
            }
        });
        dirty.set(true);
    };
    let day_chips = WEEKDAYS
        .into_iter()
        .map(|day| {
            let on = w.days.contains(&day);
            view! {
                <button class="day-chip" class:active=on aria-pressed=on.to_string() on:click=move |_| edit(Box::new(move |w| {
                    if let Some(p) = w.days.iter().position(|d| *d == day) {
                        w.days.remove(p);
                    } else {
                        w.days.push(day);
                        w.days.sort_by_key(|d| d.num_days_from_monday());
                    }
                }))>{format!("{day}").chars().next().unwrap_or('?').to_string()}</button>
            }
        })
        .collect_view();
    let parse = |ev: &leptos::ev::Event| NaiveTime::parse_from_str(&event_target_value(ev), "%H:%M").ok();
    view! {
        <div class="downtime">
            <div class="days">{day_chips}</div>
            <div class="times">
                <input type="time" prop:value=time_value(w.start) on:change=move |ev| {
                    if let Some(t) = parse(&ev) { edit(Box::new(move |w| w.start = t)) }
                }/>
                <span>"to"</span>
                <input type="time" prop:value=time_value(w.end) on:change=move |ev| {
                    if let Some(t) = parse(&ev) { edit(Box::new(move |w| w.end = t)) }
                }/>
                <button class="icon-button" aria-label="Remove downtime" on:click=move |_| {
                    downtime.update(|d| { if index < d.len() { d.remove(index); } });
                    dirty.set(true);
                }>"✕"</button>
            </div>
        </div>
    }
}

/// One local account on a computer, with who it counts as.
#[component]
fn AccountRowView(host: String, a: api::AccountRow) -> impl IntoView {
    let ctx = ctx();
    let kids = move || ctx.overview.with(|o| o.as_ref().map(|o| o.users.clone()).unwrap_or_default());
    if a.admin {
        return view! {
            <div class="account">
                <span class="account-name">{a.name.clone()}</span>
                <span class="muted small">"Administrator · never limited"</span>
            </div>
        }
        .into_any();
    }
    let current = a.kid.clone().unwrap_or_default();
    let account = a.name.clone();
    let change = move |ev: leptos::ev::Event| {
        let value = event_target_value(&ev);
        let (host, account) = (host.clone(), account.clone());
        let kid = (!value.is_empty()).then_some(value);
        let done = match &kid {
            Some(k) => format!("{account} now uses {}'s time", kid_name(ctx, k)),
            None => format!("{account} is not limited"),
        };
        ctx.act(done, async move { api::set_account(&host, &account, kid.as_deref()).await });
    };
    view! {
        <div class="account">
            <span class="account-name">{a.name.clone()}
                {a.new.then(|| view! { <b class="badge">"New"</b> })}
            </span>
            <select aria-label=format!("Who uses {}", a.name) on:change=change>
                <option value="" selected=current.is_empty()>"Not limited"</option>
                {move || kids().into_iter().map(|k| {
                    let selected = k.username == current;
                    view! { <option value=k.username.clone() selected=selected>{k.name.clone()}</option> }
                }).collect_view()}
            </select>
        </div>
    }
    .into_any()
}

#[component]
fn MachinesView() -> impl IntoView {
    let ctx = ctx();
    let clients = move || ctx.overview.with(|o| o.as_ref().map(|o| o.clients.clone()).unwrap_or_default());
    let hostname = RwSignal::new(String::new());
    let issued = RwSignal::new(None::<api::NewClientToken>);
    let add = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let host = hostname.get_untracked().trim().to_owned();
        if host.is_empty() {
            return;
        }
        spawn_local(async move {
            match api::add_client(&host).await {
                Ok(t) => {
                    hostname.set(String::new());
                    issued.set(Some(t));
                    ctx.refresh();
                }
                Err(ApiError::Unauthorized) => ctx.set_auth(Auth::SignedOut),
                Err(e) => ctx.say(e.to_string(), false),
            }
        });
    };
    view! {
        <header class="page-head"><h1>"Computers"</h1></header>
        <div class="card list">
            <Show when=move || !clients().is_empty() fallback=|| view! { <p class="empty">"No computers yet."</p> }>
                <For each=clients key=|c| format!("{c:?}") let:c>
                    {
                        let host = c.hostname.clone();
                        let accounts = c.accounts.clone();
                        let host_for_accounts = c.hostname.clone();
                        view! {
                            <div class="computer">
                                <div class="list-row">
                                    <div>
                                        <strong>{c.hostname.clone()}</strong>
                                        <div class="muted small">{c.last_seen.as_deref().map(|t| format!("Last check-in {}", local_date_time(t))).unwrap_or_else(|| "Never checked in".into())}</div>
                                    </div>
                                    <button class="ghost danger" on:click=move |_| {
                                        let host = host.clone();
                                        if confirm_action(&format!("Remove {host}? It will stop getting schedules until you add it again.")) {
                                            ctx.act(format!("Removed {host}"), async move { api::remove_client(&host).await });
                                        }
                                    }>"Remove"</button>
                                </div>
                                <div class="accounts">
                                    {accounts.into_iter().map(|a| view! { <AccountRowView host=host_for_accounts.clone() a=a/> }).collect_view()}
                                </div>
                            </div>
                        }
                    }
                </For>
            </Show>
        </div>
        <form class="card add-row" on:submit=add>
            <label for="host">"Add a computer"</label>
            <div class="row">
                <input id="host" placeholder="Its hostname, e.g. kids-pc" autocapitalize="none" autocomplete="off" spellcheck="false"
                    prop:value=move || hostname.get() on:input=move |ev| hostname.set(event_target_value(&ev))/>
                <button class="primary">"Add"</button>
            </div>
        </form>
        {move || issued.get().map(|t| {
            let token = t.token.clone();
            view! {
                <div class="card token">
                    <h3>{format!("Token for {}", t.hostname)}</h3>
                    <p class="muted small">"Shown only once. Put it in a file on that computer and point services.screentime.client.tokenFile at it."</p>
                    <code class="token-value">{t.token.clone()}</code>
                    <div class="row">
                        <button class="ghost" on:click=move |_| {
                            let token = token.clone();
                            spawn_local(async move {
                                match copy_text(&token).await {
                                    Ok(_) => ctx.say("Copied", true),
                                    Err(e) => ctx.say(js_error(e), false),
                                }
                            });
                        }>"Copy"</button>
                        <button class="ghost" on:click=move |_| issued.set(None)>"Done"</button>
                    </div>
                </div>
            }
        })}
    }
}

#[component]
fn NotificationsView() -> impl IntoView {
    let ctx = ctx();
    let support = push_support();
    let enabled = RwSignal::new(false);
    let devices = RwSignal::new(None::<i64>);
    let busy = RwSignal::new(false);
    let load = move || {
        spawn_local(async move {
            enabled.set(push_enabled().await.as_bool().unwrap_or(false));
            if let Ok(info) = api::push_info().await {
                devices.set(Some(info.subscriptions));
            }
        })
    };
    load();

    let enable = move |_| {
        busy.set(true);
        spawn_local(async move {
            let result = async {
                let info = api::push_info().await.map_err(|e| e.to_string())?;
                let sub = enable_push(&info.public_key).await.map_err(js_error)?;
                let sub = sub.as_string().ok_or("No subscription came back.")?;
                api::push_subscribe(&sub, &device_label()).await.map_err(|e| e.to_string())
            }
            .await;
            match result {
                Ok(()) => ctx.say("Notifications are on for this device", true),
                Err(e) => ctx.say(e, false),
            }
            busy.set(false);
            load();
        });
    };
    let test = move |_| {
        spawn_local(async move {
            match api::push_test().await {
                Ok(0) => ctx.say("No devices have notifications on yet.", false),
                Ok(n) => ctx.say(format!("Sent to {n} device{}", if n == 1 { "" } else { "s" }), true),
                Err(e) => ctx.say(e.to_string(), false),
            }
        });
    };

    let body = match support.as_str() {
        "install" => view! {
            <p>"On iPhone and iPad, notifications only work from the Home Screen app:"</p>
            <ol class="steps">
                <li>"Tap the Share button in Safari."</li>
                <li>"Choose “Add to Home Screen”."</li>
                <li>"Open Screen time from your Home Screen and come back here."</li>
            </ol>
        }.into_any(),
        "insecure" => view! {
            <p>"Notifications need a secure (HTTPS) connection that this device trusts. If you use your own certificate, install its ca.pem on this device first."</p>
        }.into_any(),
        "unsupported" => view! { <p>"This browser can't receive web notifications. Try Chrome on Android or Safari on iPhone."</p> }.into_any(),
        _ => view! {
            <p>{move || if enabled.get() {
                "This device gets a notification whenever someone asks for more time."
            } else {
                "Turn on notifications to hear about time requests on this device, and answer them from the notification."
            }}</p>
            <div class="row">
                <button class="primary" disabled=move || busy.get() || enabled.get() on:click=enable>
                    {move || if enabled.get() { "On for this device" } else if busy.get() { "Turning on…" } else { "Turn on notifications" }}
                </button>
                <button class="ghost" on:click=test>"Send a test"</button>
            </div>
        }.into_any(),
    };

    view! {
        <header class="page-head"><h1>"Alerts"</h1></header>
        <div class="card">
            <h3>"Notifications"</h3>
            {body}
            <p class="muted small">{move || devices.get().map(|n| match n {
                0 => "No devices are signed up yet.".to_owned(),
                1 => "1 device is signed up.".to_owned(),
                n => format!("{n} devices are signed up."),
            })}</p>
        </div>
    }
}

#[component]
fn AccountView() -> impl IntoView {
    let ctx = ctx();
    let current = RwSignal::new(String::new());
    let new = RwSignal::new(String::new());
    let confirm = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let change = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let (cur, new_pw) = (current.get_untracked(), new.get_untracked());
        if new_pw != confirm.get_untracked() {
            error.set(Some("The new passwords don't match.".into()));
            return;
        }
        if new_pw.chars().count() < 7 {
            error.set(Some("Use at least 7 characters.".into()));
            return;
        }
        error.set(None);
        busy.set(true);
        spawn_local(async move {
            match api::change_password(&cur, &new_pw).await {
                Ok(()) => {
                    for field in [current, new, confirm] {
                        field.set(String::new());
                    }
                    ctx.say("Password changed. Other devices were signed out.", true);
                }
                Err(ApiError::Unauthorized) => ctx.set_auth(Auth::SignedOut),
                Err(e) => error.set(Some(e.to_string())),
            }
            busy.set(false);
        });
    };
    let logout = move |_| {
        spawn_local(async move {
            let _ = api::logout().await;
            ctx.overview.set(None);
            ctx.set_auth(Auth::SignedOut);
        });
    };
    let field = |id: &'static str, label: &'static str, autocomplete: &'static str, value: RwSignal<String>| {
        view! {
            <label for=id>{label}</label>
            <input id=id type="password" autocomplete=autocomplete required
                prop:value=move || value.get() on:input=move |ev| value.set(event_target_value(&ev))/>
        }
    };

    view! {
        <header class="page-head"><h1>"Account"</h1></header>
        <form class="card form" on:submit=change>
            <h3>"Change password"</h3>
            {field("pw-current", "Current password", "current-password", current)}
            {field("pw-new", "New password", "new-password", new)}
            {field("pw-confirm", "New password again", "new-password", confirm)}
            <p class="muted small">"At least 7 characters. It's also what you type at a computer for \u{201c}My parent is here\u{201d}. Changing it signs out every other device, and the computers take the new one within a minute."</p>
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="row">
                <button class="primary" disabled=move || busy.get()>
                    {move || if busy.get() { "Changing…" } else { "Change password" }}
                </button>
            </div>
        </form>
        <div class="card">
            <h3>"Sign out"</h3>
            <p class="muted small">"Signs out this device only."</p>
            <button class="ghost" on:click=logout>"Sign out"</button>
        </div>
    }
}
