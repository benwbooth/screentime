//! Sessions through the WTS API, locking (LockWorkStation run in the kid's session), logging off,
//! Windows logon hours,
//! the admin password check, and keeping the tray app running in the kid's session.

use super::{from_wide, pcwstr, wide};
use crate::logon_hours;
use screentime_client::agent::{Platform, Session};
use screentime_client::state::State;
use screentime_common::LocalAccount;
use screentime_common::Decision;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::NetworkManagement::NetManagement::{
    FILTER_NORMAL_ACCOUNT, LOCALGROUP_MEMBERS_INFO_2, NetApiBufferFree, NetLocalGroupGetMembers, NetUserEnum,
    NetUserGetInfo, NetUserSetInfo,
    UF_ACCOUNTDISABLE, USER_INFO_1, USER_INFO_24, USER_INFO_1020, USER_PRIV_ADMIN,
};
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, LOGON32_LOGON, LOGON32_LOGON_INTERACTIVE, LOGON32_LOGON_NETWORK,
    LOGON32_PROVIDER_DEFAULT, LogonUserW,
    LookupAccountSidW, PSID, SID_NAME_USE, SidTypeUser,
    SECURITY_MAX_SID_SIZE, TOKEN_GROUPS, TokenGroups, WinBuiltinAdministratorsSid, CreateWellKnownSid,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    WTS_SESSION_INFOW, WTSActive, WTSDisconnectSession, WTSDisconnected, WTSEnumerateSessionsW,
    WTSFreeMemory, WTSINFOEXW, WTSLogoffSession, WTSQuerySessionInformationW, WTSQueryUserToken,
    WTSSessionInfoEx, WTSUserName, WTS_SESSIONSTATE_LOCK, ProcessIdToSessionId,
};
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

/// Hours in Windows' weekly logon-hours map.
const UNITS_PER_WEEK: u32 = 168;
const TRAY_EXE: &str = "screentime-tray.exe";
/// Don't try to start the tray in a session more often than this.
/// How soon the tray app is started again after it's closed (it runs as the kid, who can end it
/// in Task Manager).
const TRAY_RETRY: Duration = Duration::from_secs(15);
/// How long a session may keep refusing to lock before it is disconnected instead.
const LOCK_FALLBACK: Duration = Duration::from_secs(60);

pub struct Windows {
    /// Mirror the schedule into Windows logon hours (on unless SCREENTIME_LOGON_HOURS=off).
    use_logon_hours: bool,
    /// Last logon-hours map written for each user, so Windows is only told about changes.
    logon_hours: Mutex<HashMap<String, [u8; logon_hours::BYTES]>>,
    tray_started: Mutex<HashMap<u32, Instant>>,
    /// When we first asked each session to lock and it still hasn't, for the disconnect fallback.
    lock_pending: Mutex<HashMap<u32, Instant>>,
}

impl Windows {
    /// `users` are the configured accounts, only checked here so a misconfigured admin is reported.
    pub fn new(users: Vec<String>) -> Windows {
        for user in &users {
            if is_admin_account(user) {
                tracing::error!(user, "this account is an administrator; it can't be limited, so it will be ignored");
            }
        }
        let use_logon_hours = !std::env::var("SCREENTIME_LOGON_HOURS").is_ok_and(|v| v.eq_ignore_ascii_case("off"));
        if !use_logon_hours {
            tracing::warn!("logon hours are off: sign-in is only blocked while the service is running");
        }
        Windows { use_logon_hours, logon_hours: Mutex::default(), tray_started: Mutex::default(), lock_pending: Mutex::default() }
    }

    fn ensure_tray(&self, session_id: u32) {
        if tray_running_in(session_id) {
            return;
        }
        let mut started = self.tray_started.lock().unwrap();
        if started.get(&session_id).is_some_and(|at| at.elapsed() < TRAY_RETRY) {
            return;
        }
        started.insert(session_id, Instant::now());
        match start_in_session(session_id, "") {
            Ok(()) => tracing::info!(session_id, "started the tray app"),
            Err(e) => tracing::warn!(session_id, "couldn't start the tray app: {e:#}"),
        }
    }
}

impl Platform for Windows {
    async fn sessions(&self) -> Result<Vec<Session>> {
        let sessions = list_sessions()?;
        // A lock that took is done; the next unlock starts the fallback clock afresh.
        self.lock_pending
            .lock()
            .unwrap()
            .retain(|id, _| sessions.iter().any(|s| s.id == id.to_string() && !s.locked));
        Ok(sessions)
    }

    fn on_allowed_session(&self, session: &Session) {
        self.lock_pending.lock().unwrap().remove(&session.id.parse().unwrap_or(0));
        // Only into sessions that are staying: starting it in one being shut out crashed it.
        self.ensure_tray(session.id.parse().unwrap_or(0));
    }

    fn local_accounts(&self) -> Vec<LocalAccount> {
        local_accounts()
    }

    fn release_user(&self, user: &str) {
        self.logon_hours.lock().unwrap().remove(user);
        if self.use_logon_hours && !is_admin_account(user) {
            match set_logon_hours(user, [0xff; logon_hours::BYTES]) {
                Ok(()) => tracing::info!(user, "sign-in hours reset to any time"),
                Err(e) => tracing::error!(user, "resetting sign-in hours failed: {e:#}"),
            }
        }
    }

    async fn lock(&self, session: &Session) -> Result<()> {
        // A service can't call LockWorkStation for another session, so run the tray exe as the
        // kid with `--lock`, which locks and exits: the ordinary lock screen, with everything
        // (games included) still running. Never disconnect a session that is still signing in:
        // Windows abandons the logon and signs the kid out, which is what kids saw as being
        // logged out. Only if locking keeps failing on an established session is it disconnected.
        let id: u32 = session.id.parse()?;
        let first = *self.lock_pending.lock().unwrap().entry(id).or_insert_with(Instant::now);
        let started = start_in_session(id, "--lock");
        if first.elapsed() >= LOCK_FALLBACK {
            tracing::warn!(session_id = id, "session still not locked, disconnecting it instead");
            self.lock_pending.lock().unwrap().remove(&id);
            return unsafe { WTSDisconnectSession(None, id, false) }.context("WTSDisconnectSession");
        }
        started.context("starting the lock helper")
    }

    async fn end_sessions(&self, _user: &str, sessions: &[Session]) -> Result<()> {
        for s in sessions {
            let id: u32 = s.id.parse()?;
            unsafe { WTSLogoffSession(None, id, false) }.context("WTSLogoffSession")?;
        }
        Ok(())
    }

    fn admins(&self) -> Vec<String> {
        local_admins()
    }

    fn sync_os_restrictions(&self, user: &str, state: &State, now: DateTime<Utc>) {
        if !self.use_logon_hours || is_admin_account(user) {
            return;
        }
        let map = logon_hours::weekly_map(
            now,
            |t| t.with_timezone(&Local).naive_local(),
            |t| matches!(state.decide(user, t).0, Decision::Allowed { .. }),
        );
        let mut written = self.logon_hours.lock().unwrap();
        if written.get(user) == Some(&map) {
            return;
        }
        match set_logon_hours(user, map) {
            Ok(()) => {
                tracing::info!(user, "updated Windows logon hours");
                written.insert(user.to_owned(), map);
            }
            Err(e) => tracing::error!(user, "setting logon hours failed: {e:#}"),
        }
    }
}

fn query_string(session_id: u32, class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS) -> Option<String> {
    let mut buf = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        WTSQuerySessionInformationW(None, session_id, class, &mut buf, &mut len).ok()?;
        let s = buf.to_string().ok();
        WTSFreeMemory(buf.0 as _);
        s
    }
}

fn is_locked(session_id: u32) -> bool {
    let mut buf = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        if WTSQuerySessionInformationW(None, session_id, WTSSessionInfoEx, &mut buf, &mut len).is_err() {
            return false;
        }
        let info = &*(buf.0 as *const WTSINFOEXW);
        let flags = info.Data.WTSInfoExLevel1.SessionFlags as u32;
        WTSFreeMemory(buf.0 as _);
        flags == WTS_SESSIONSTATE_LOCK
    }
}

/// Signed-in user sessions: the console and any remote ones, connected or not.
fn list_sessions() -> Result<Vec<Session>> {
    let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut info, &mut count) }.context("WTSEnumerateSessionsW")?;
    let mut out = Vec::new();
    for i in 0..count as usize {
        let s = unsafe { &*info.add(i) };
        if s.SessionId == 0 || (s.State != WTSActive && s.State != WTSDisconnected) {
            continue;
        }
        let Some(user) = query_string(s.SessionId, WTSUserName).filter(|u| !u.is_empty()) else { continue };
        let locked = s.State == WTSDisconnected || is_locked(s.SessionId);
        out.push(Session { id: s.SessionId.to_string(), user, in_use: !locked, locked });
    }
    unsafe { WTSFreeMemory(info as _) };
    Ok(out)
}

/// Enabled local user accounts, and whether each is an administrator.
pub fn local_accounts() -> Vec<LocalAccount> {
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total) = (0u32, 0u32);
    let rc = unsafe { NetUserEnum(None, 1, FILTER_NORMAL_ACCOUNT, &mut buf, u32::MAX, &mut read, &mut total, None) };
    if rc != 0 || buf.is_null() {
        return Vec::new();
    }
    let users = unsafe { std::slice::from_raw_parts(buf as *const USER_INFO_1, read as usize) };
    let out = users
        .iter()
        .filter(|u| u.usri1_flags & UF_ACCOUNTDISABLE != UF_ACCOUNTDISABLE)
        .filter_map(|u| {
            let name = unsafe { u.usri1_name.to_string() }.ok()?;
            Some(LocalAccount { name, admin: u.usri1_priv == USER_PRIV_ADMIN, configured_kid: None })
        })
        .collect();
    unsafe { NetApiBufferFree(Some(buf as _)) };
    out
}

/// Enabled local user accounts in BUILTIN\Administrators (looked up by SID, so it works
/// whatever the group is called in this Windows language).
pub fn local_admins() -> Vec<String> {
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = sid.len() as u32;
    let psid = PSID(sid.as_mut_ptr() as _);
    if unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, None, Some(psid), &mut size) }.is_err() {
        return Vec::new();
    }
    let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
    let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
    let mut use_ = SID_NAME_USE::default();
    if unsafe {
        LookupAccountSidW(
            None,
            psid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut use_,
        )
    }
    .is_err()
    {
        return Vec::new();
    }
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total) = (0u32, 0u32);
    let rc = unsafe {
        NetLocalGroupGetMembers(None, PCWSTR(name.as_ptr()), 2, &mut buf, u32::MAX, &mut read, &mut total, None)
    };
    if rc != 0 || buf.is_null() {
        return Vec::new();
    }
    let members = unsafe { std::slice::from_raw_parts(buf as *const LOCALGROUP_MEMBERS_INFO_2, read as usize) };
    let computer = std::env::var("COMPUTERNAME").unwrap_or_default();
    let mut out = Vec::new();
    for m in members {
        if m.lgrmi2_sidusage != SidTypeUser {
            continue;
        }
        let full = unsafe { m.lgrmi2_domainandname.to_string() }.unwrap_or_default();
        let Some((domain, user)) = full.split_once('\\') else { continue };
        if domain.eq_ignore_ascii_case(&computer) && !is_disabled(user) {
            out.push(user.to_owned());
        }
    }
    unsafe { NetApiBufferFree(Some(buf as _)) };
    out
}

fn is_disabled(user: &str) -> bool {
    let name = wide(user);
    let mut buf: *mut u8 = std::ptr::null_mut();
    unsafe {
        if NetUserGetInfo(None, pcwstr(&name), 1, &mut buf) != 0 || buf.is_null() {
            return true;
        }
        let disabled = (*(buf as *const USER_INFO_1)).usri1_flags & UF_ACCOUNTDISABLE == UF_ACCOUNTDISABLE;
        NetApiBufferFree(Some(buf as _));
        disabled
    }
}

/// Whether a local account has administrator rights (USER_PRIV_ADMIN).
pub fn is_admin_account(user: &str) -> bool {
    let name = wide(user);
    let mut buf: *mut u8 = std::ptr::null_mut();
    unsafe {
        if NetUserGetInfo(None, pcwstr(&name), 1, &mut buf) != 0 || buf.is_null() {
            return false;
        }
        let admin = (*(buf as *const USER_INFO_1)).usri1_priv == USER_PRIV_ADMIN;
        NetApiBufferFree(Some(buf as _));
        admin
    }
}

fn set_logon_hours(user: &str, mut map: [u8; logon_hours::BYTES]) -> Result<()> {
    let name = wide(user);
    let info = USER_INFO_1020 { usri1020_units_per_week: UNITS_PER_WEEK, usri1020_logon_hours: map.as_mut_ptr() };
    let rc = unsafe { NetUserSetInfo(None, pcwstr(&name), 1020, &info as *const _ as *const u8, None) };
    if rc == 0 { Ok(()) } else { Err(anyhow!("NetUserSetInfo returned {rc}")) }
}

/// Check `admin`'s password and that the account is an administrator.
/// Accepts `name`, `COMPUTER\name`, `MicrosoftAccount\email` or an email. A local account
/// linked to a Microsoft account signs in with the Microsoft account's password, which only
/// works under its Microsoft name, so that is tried too.
pub fn verify_admin(admin: &str, password: &str) -> Result<(), String> {
    let (domain, user) = match admin.split_once('\\') {
        Some((d, u)) => (Some(d.to_owned()), u.to_owned()),
        None if admin.contains('@') => (Some("MicrosoftAccount".to_owned()), admin.to_owned()),
        None => (Some(".".to_owned()), admin.to_owned()),
    };
    // Every form Windows may accept this password under: as given, and for a local account
    // linked to a Microsoft account, as MicrosoftAccount\email and as a bare email (UPN).
    let mut names = vec![(domain.clone(), user.clone())];
    if domain.as_deref() == Some(".")
        && let Some(email) = microsoft_account_of(&user)
    {
        names.push((Some("MicrosoftAccount".to_owned()), email.clone()));
        names.push((None, email));
    }
    let mut last_err = String::new();
    for (domain, user) in &names {
        for kind in [LOGON32_LOGON_NETWORK, LOGON32_LOGON_INTERACTIVE] {
            match logon(domain.as_deref(), user, password, kind) {
                Ok(token) => {
                    let admin_ok = token_is_admin(token);
                    unsafe {
                        let _ = CloseHandle(token);
                    }
                    return if admin_ok? { Ok(()) } else { Err(format!("{user} is not an administrator")) };
                }
                Err(e) => {
                    tracing::debug!(?domain, user, kind = kind.0, "parent sign-in attempt failed: {e}");
                    last_err = e.to_string();
                }
            }
        }
    }
    Err(format!("sign-in failed: {last_err}"))
}

fn logon(domain: Option<&str>, user: &str, password: &str, kind: LOGON32_LOGON) -> windows::core::Result<HANDLE> {
    let (user_w, pass_w) = (wide(user), wide(password));
    let domain_w = domain.map(wide);
    let mut token = HANDLE::default();
    unsafe {
        LogonUserW(
            pcwstr(&user_w),
            domain_w.as_deref().map(pcwstr).unwrap_or(PCWSTR::null()),
            pcwstr(&pass_w),
            kind,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    }?;
    Ok(token)
}

/// The Microsoft account (email) a local account is linked to, if any.
fn microsoft_account_of(user: &str) -> Option<String> {
    let name = wide(user);
    let mut buf: *mut u8 = std::ptr::null_mut();
    unsafe {
        if NetUserGetInfo(None, pcwstr(&name), 24, &mut buf) != 0 || buf.is_null() {
            return None;
        }
        let info = &*(buf as *const USER_INFO_24);
        let email = (info.usri24_internet_identity.as_bool() && !info.usri24_internet_principal_name.is_null())
            .then(|| info.usri24_internet_principal_name.to_string().ok())
            .flatten();
        NetApiBufferFree(Some(buf as _));
        email.filter(|e| !e.is_empty())
    }
}

/// Whether the token's groups include BUILTIN\Administrators. With UAC the group is marked
/// deny-only in a filtered token, but its presence still means the account is an administrator.
fn token_is_admin(token: HANDLE) -> Result<bool, String> {
    let mut admins = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = admins.len() as u32;
    let admins_sid = PSID(admins.as_mut_ptr() as _);
    unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, None, Some(admins_sid), &mut size) }
        .map_err(|e| e.to_string())?;

    let mut needed = 0u32;
    let _ = unsafe { GetTokenInformation(token, TokenGroups, None, 0, &mut needed) };
    let mut buf = vec![0u8; needed as usize];
    unsafe { GetTokenInformation(token, TokenGroups, Some(buf.as_mut_ptr() as _), needed, &mut needed) }
        .map_err(|e| e.to_string())?;
    let groups = unsafe { &*(buf.as_ptr() as *const TOKEN_GROUPS) };
    let entries = unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    Ok(entries.iter().any(|g| unsafe { EqualSid(g.Sid, admins_sid) }.is_ok()))
}

fn tray_running_in(session_id: u32) -> bool {
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else { return false };
    let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut found = false;
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        if from_wide(&entry.szExeFile).eq_ignore_ascii_case(TRAY_EXE) {
            let mut sid = 0u32;
            if unsafe { ProcessIdToSessionId(entry.th32ProcessID, &mut sid) }.is_ok() && sid == session_id {
                found = true;
                break;
            }
        }
        ok = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }
    unsafe {
        let _ = CloseHandle(snap);
    }
    found
}

/// Start the tray app as the session's user, next to this executable.
/// The tray app runs as the kid (so it sees their desktop, taskbar and settings) but they may only
/// look at it: no ending, suspending or debugging it from Task Manager. OW stops the owner's
/// implicit right to rewrite this list.
const TRAY_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x101000;;;OW)";

fn start_in_session(session_id: u32, args: &str) -> Result<()> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    let exe = std::env::current_exe()?.with_file_name(TRAY_EXE);
    let mut cmd = wide(format!("\"{}\" {args}", exe.display()).trim_end());
    let mut desktop = wide(r"winsta0\default");
    let sddl = wide(TRAY_SDDL);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(pcwstr(&sddl), SDDL_REVISION_1, &mut descriptor, None) }
        .context("tray security descriptor")?;
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let result = unsafe { start_with(session_id, &mut cmd, &mut desktop, &attrs) };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result
}

unsafe fn start_with(
    session_id: u32,
    cmd: &mut [u16],
    desktop: &mut [u16],
    attrs: &windows::Win32::Security::SECURITY_ATTRIBUTES,
) -> Result<()> {
    unsafe {
        let mut token = HANDLE::default();
        WTSQueryUserToken(session_id, &mut token).context("WTSQueryUserToken")?;
        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        let env_ok = CreateEnvironmentBlock(&mut env, Some(token), false).is_ok();
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        let result = CreateProcessAsUserW(
            Some(token),
            windows::core::PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            Some(attrs),
            Some(attrs),
            false,
            CREATE_UNICODE_ENVIRONMENT,
            env_ok.then_some(env as *const _),
            windows::core::PCWSTR::null(),
            &startup,
            &mut process,
        );
        if env_ok {
            let _ = DestroyEnvironmentBlock(env);
        }
        let _ = CloseHandle(token);
        result.context("CreateProcessAsUserW")?;
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

