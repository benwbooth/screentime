use screentime_common::{CLIENT_PIPE, Decision, DenyReason, SocketRequest, SocketResponse, UserStatus};
use std::cell::RefCell;
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::time::Duration;
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_UNEXPECTED, NTSTATUS,
    S_FALSE,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, HBITMAP,
    SelectObject,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW};
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result, implement};
use windows_core::IUnknownImpl;

pub const CLSID: GUID = GUID::from_u128(0x6f6b7a8e_2d1c_4b8a_9a53_7c0e2f4a91d3);

const PIPE_TIMEOUT: Duration = Duration::from_secs(4);
/// The kid last picked on the tile, so it's preselected next time (LogonUI runs as SYSTEM,
/// which may write here; kids can't read or write this folder).
const LAST_WHO_FILE: &str = r"C:\ProgramData\screentime\tile-last-user.txt";
const ADD_AMOUNTS: [u32; 5] = [15, 30, 45, 60, 120];

// Field IDs, in display order.
const F_IMAGE: u32 = 0;
const F_TITLE: u32 = 1;
const F_STATUS: u32 = 2;
const F_WHO: u32 = 3;
const F_ASK: u32 = 4;
const F_PARENT_LABEL: u32 = 5;
const F_ADMIN: u32 = 6;
const F_PASSWORD: u32 = 7;
const F_ADD: u32 = 8;
const F_SUBMIT: u32 = 9;
const FIELD_COUNT: u32 = 10;

fn fmt_minutes(m: u32) -> String {
    match (m / 60, m % 60) {
        (0, m) => format!("{m} minutes"),
        (1, 0) => "1 hour".into(),
        (h, 0) => format!("{h} hours"),
        (h, m) => format!("{h} h {m} min"),
    }
}

fn field(id: u32) -> (CREDENTIAL_PROVIDER_FIELD_TYPE, String) {
    match id {
        F_IMAGE => (CPFT_TILE_IMAGE, "Screen time".into()),
        F_TITLE => (CPFT_LARGE_TEXT, "Screen time".into()),
        F_STATUS => (CPFT_SMALL_TEXT, String::new()),
        F_WHO => (CPFT_COMBOBOX, "Who".into()),
        F_ASK => (CPFT_COMMAND_LINK, "Request more time".into()),
        F_PARENT_LABEL => (CPFT_SMALL_TEXT, "My parent is here".into()),
        F_ADMIN => (CPFT_COMBOBOX, "Parent".into()),
        F_PASSWORD => (CPFT_PASSWORD_TEXT, "Screen time password".into()),
        F_ADD => (CPFT_COMBOBOX, "Add".into()),
        _ => (CPFT_SUBMIT_BUTTON, "Add time".into()),
    }
}

/// A copy of `s` the caller frees with CoTaskMemFree, as LogonUI expects.
fn co_string(s: &str) -> Result<PWSTR> {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let p = CoTaskMemAlloc(w.len() * 2) as *mut u16;
        if p.is_null() {
            return Err(E_FAIL.into());
        }
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        Ok(PWSTR(p))
    }
}

/// Run `f`, turning a panic into an error instead of taking down LogonUI.
fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(E_UNEXPECTED.into()))
}

/// One request to the service, on a worker thread so a stuck pipe can't freeze the sign-in screen.
fn ask(request: SocketRequest) -> std::result::Result<SocketResponse, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> std::result::Result<SocketResponse, String> {
            let mut pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(CLIENT_PIPE)
                .map_err(|_| "The screen time service isn't running.".to_string())?;
            let mut line = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
            line.push(b'\n');
            pipe.write_all(&line).map_err(|e| e.to_string())?;
            let mut reply = String::new();
            BufReader::new(pipe).read_line(&mut reply).map_err(|e| e.to_string())?;
            serde_json::from_str(&reply).map_err(|e| e.to_string())
        })();
        let _ = tx.send(result);
    });
    rx.recv_timeout(PIPE_TIMEOUT).unwrap_or_else(|_| Err("The screen time service didn't answer.".into()))
}

fn describe(u: &UserStatus) -> String {
    match &u.decision {
        Decision::Allowed { remaining_minutes: Some(m) } => format!("{}: {} left today", u.username, fmt_minutes(*m)),
        Decision::Allowed { remaining_minutes: None } => format!("{}: no limit right now", u.username),
        Decision::Denied { reason: DenyReason::Downtime { until } } => {
            format!("{}: downtime until {}", u.username, until.format("%-I:%M %p"))
        }
        Decision::Denied { reason: DenyReason::OutOfTime } => format!("{}: out of screen time for today", u.username),
        Decision::Denied { reason: DenyReason::NoPolicy } => format!("{}: waiting for settings", u.username),
        Decision::Denied { reason: DenyReason::Locked } => format!("{}: locked by a parent", u.username),
    }
}

fn reply_text(r: std::result::Result<SocketResponse, String>) -> (String, bool) {
    match r {
        Ok(SocketResponse::RequestSent) => ("Request sent. Your parent will get a notification.".into(), true),
        Ok(SocketResponse::RequestQueued) => ("Request saved; it will be sent when the computer is online.".into(), true),
        Ok(SocketResponse::Granted { minutes }) => (format!("Added {}. Sign in to continue.", fmt_minutes(minutes)), true),
        Ok(SocketResponse::Error { message }) => (message, false),
        Ok(SocketResponse::Unmanaged) => ("That account has no screen time limits.".into(), false),
        Ok(_) => ("Unexpected answer from the screen time service.".into(), false),
        Err(e) => (e, false),
    }
}

// ---------------------------------------------------------------------------------------------

#[implement(ICredentialProviderCredential)]
struct Credential {
    users: RefCell<Vec<UserStatus>>,
    who: RefCell<u32>,
    add: RefCell<u32>,
    status: RefCell<String>,
    /// Accounts that can approve time, and which one is picked (the last one used, at first).
    admins: RefCell<Vec<String>>,
    admin_sel: RefCell<u32>,
    password: RefCell<String>,
    /// Whether the PIN box was typed into one key at a time from empty since it was last
    /// cleared. The sign-in screen can keep a box's text after a parent used it; editing that
    /// leftover would otherwise hand a kid the parent's PIN without knowing it.
    typed_fresh: RefCell<bool>,
    events: RefCell<Option<ICredentialProviderCredentialEvents>>,
}

impl Credential {
    fn new(users: Vec<UserStatus>) -> Credential {
        let status = users.first().map(describe).unwrap_or_default();
        let (admins, last) = match ask(SocketRequest::ListAdmins) {
            Ok(SocketResponse::Admins { admins, last }) => (admins, last),
            _ => (Vec::new(), None),
        };
        let admin_sel = last.and_then(|l| admins.iter().position(|a| *a == l)).unwrap_or(0) as u32;
        let last_who = std::fs::read_to_string(LAST_WHO_FILE).unwrap_or_default();
        let who = users.iter().position(|u| u.username.eq_ignore_ascii_case(last_who.trim())).unwrap_or(0) as u32;
        let status = users.get(who as usize).map(describe).unwrap_or(status);
        Credential {
            users: RefCell::new(users),
            who: RefCell::new(who),
            add: RefCell::new(1),
            status: RefCell::new(status),
            admins: RefCell::new(admins),
            admin_sel: RefCell::new(admin_sel),
            password: RefCell::default(),
            typed_fresh: RefCell::new(true),
            events: RefCell::default(),
        }
    }

    fn selected_user(&self) -> Option<String> {
        self.users.borrow().get(*self.who.borrow() as usize).map(|u| u.username.clone())
    }

    fn refresh(&self) {
        if let Ok(SocketResponse::Users { users }) = ask(SocketRequest::ListUsers)
            && !users.is_empty()
        {
            *self.users.borrow_mut() = users;
        }
        let text = self.users.borrow().get(*self.who.borrow() as usize).map(describe).unwrap_or_default();
        *self.status.borrow_mut() = text;
    }
}

impl Credential_Impl {
    /// Push a new string to the tile, if LogonUI is listening.
    fn show(&self, field: u32, text: &str) {
        if let Some(events) = self.events.borrow().as_ref() {
            let me: ICredentialProviderCredential = self.to_interface();
            let w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                let _ = events.SetFieldString(&me, field, PCWSTR(w.as_ptr()));
            }
        }
    }

    fn set_status(&self, text: &str) {
        *self.status.borrow_mut() = text.to_owned();
        self.show(F_STATUS, text);
    }
}

impl ICredentialProviderCredential_Impl for Credential_Impl {
    fn Advise(&self, events: Ref<'_, ICredentialProviderCredentialEvents>) -> Result<()> {
        guard(|| {
            *self.events.borrow_mut() = events.cloned();
            Ok(())
        })
    }

    fn UnAdvise(&self) -> Result<()> {
        guard(|| {
            *self.events.borrow_mut() = None;
            Ok(())
        })
    }

    fn SetSelected(&self) -> Result<BOOL> {
        guard(|| {
            self.refresh();
            let text = self.status.borrow().clone();
            self.show(F_STATUS, &text);
            Ok(false.into())
        })
    }

    fn SetDeselected(&self) -> Result<()> {
        guard(|| {
            self.password.borrow_mut().clear();
            *self.typed_fresh.borrow_mut() = true;
            self.show(F_PASSWORD, "");
            Ok(())
        })
    }

    fn GetFieldState(
        &self,
        id: u32,
        state: *mut CREDENTIAL_PROVIDER_FIELD_STATE,
        interactive: *mut CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
    ) -> Result<()> {
        guard(|| {
            if id >= FIELD_COUNT || state.is_null() || interactive.is_null() {
                return Err(E_INVALIDARG.into());
            }
            let s = match id {
                F_IMAGE | F_TITLE | F_STATUS => CPFS_DISPLAY_IN_BOTH,
                F_WHO if self.users.borrow().len() <= 1 => CPFS_HIDDEN,
                _ => CPFS_DISPLAY_IN_SELECTED_TILE,
            };
            unsafe {
                *state = s;
                *interactive = CPFIS_NONE;
            }
            Ok(())
        })
    }

    fn GetStringValue(&self, id: u32) -> Result<PWSTR> {
        guard(|| match id {
            F_STATUS => co_string(&self.status.borrow()),
            F_PASSWORD => co_string(""),
            id if id < FIELD_COUNT => co_string(&field(id).1),
            _ => Err(E_INVALIDARG.into()),
        })
    }

    fn GetBitmapValue(&self, id: u32) -> Result<HBITMAP> {
        guard(|| if id == F_IMAGE { tile_bitmap() } else { Err(E_INVALIDARG.into()) })
    }

    fn GetCheckboxValue(&self, _: u32, _: *mut BOOL, _: *mut PWSTR) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetSubmitButtonValue(&self, id: u32) -> Result<u32> {
        guard(|| if id == F_SUBMIT { Ok(F_PASSWORD) } else { Err(E_INVALIDARG.into()) })
    }

    fn GetComboBoxValueCount(&self, id: u32, count: *mut u32, selected: *mut u32) -> Result<()> {
        guard(|| {
            if count.is_null() || selected.is_null() {
                return Err(E_INVALIDARG.into());
            }
            let (n, sel) = match id {
                F_WHO => (self.users.borrow().len() as u32, *self.who.borrow()),
                F_ADD => (ADD_AMOUNTS.len() as u32, *self.add.borrow()),
                F_ADMIN => (self.admins.borrow().len() as u32, *self.admin_sel.borrow()),
                _ => return Err(E_INVALIDARG.into()),
            };
            unsafe {
                *count = n;
                *selected = sel;
            }
            Ok(())
        })
    }

    fn GetComboBoxValueAt(&self, id: u32, item: u32) -> Result<PWSTR> {
        guard(|| match id {
            F_WHO => match self.users.borrow().get(item as usize) {
                Some(u) => co_string(&u.username),
                None => Err(E_INVALIDARG.into()),
            },
            F_ADD => match ADD_AMOUNTS.get(item as usize) {
                Some(m) => co_string(&fmt_minutes(*m)),
                None => Err(E_INVALIDARG.into()),
            },
            F_ADMIN => match self.admins.borrow().get(item as usize) {
                Some(a) => co_string(a),
                None => Err(E_INVALIDARG.into()),
            },
            _ => Err(E_INVALIDARG.into()),
        })
    }

    fn SetStringValue(&self, id: u32, value: &PCWSTR) -> Result<()> {
        guard(|| {
            let text = if value.is_null() { String::new() } else { unsafe { value.to_string() }.unwrap_or_default() };
            match id {
                F_PASSWORD => {
                    let mut password = self.password.borrow_mut();
                    let one_key = typed_one_key(&password, &text);
                    let mut fresh = self.typed_fresh.borrow_mut();
                    *fresh = text.is_empty() || (*fresh && one_key);
                    *password = text;
                }
                _ => return Err(E_INVALIDARG.into()),
            }
            Ok(())
        })
    }

    fn SetCheckboxValue(&self, _: u32, _: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetComboBoxSelectedValue(&self, id: u32, item: u32) -> Result<()> {
        guard(|| {
            match id {
                F_WHO if (item as usize) < self.users.borrow().len() => {
                    *self.who.borrow_mut() = item;
                    if let Some(u) = self.users.borrow().get(item as usize) {
                        let _ = std::fs::write(LAST_WHO_FILE, &u.username);
                    }
                    let text = self.users.borrow().get(item as usize).map(describe).unwrap_or_default();
                    self.set_status(&text);
                }
                F_ADD if (item as usize) < ADD_AMOUNTS.len() => *self.add.borrow_mut() = item,
                F_ADMIN if (item as usize) < self.admins.borrow().len() => *self.admin_sel.borrow_mut() = item,
                _ => return Err(E_INVALIDARG.into()),
            }
            Ok(())
        })
    }

    fn CommandLinkClicked(&self, id: u32) -> Result<()> {
        guard(|| {
            if id != F_ASK {
                return Err(E_INVALIDARG.into());
            }
            let Some(username) = self.selected_user() else { return Ok(()) };
            let (text, _) = reply_text(ask(SocketRequest::RequestTime { username, minutes: None, reason: None }));
            self.set_status(&text);
            Ok(())
        })
    }

    fn GetSerialization(
        &self,
        response: *mut CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE,
        _serialization: *mut CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
        status_text: *mut PWSTR,
        status_icon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
    ) -> Result<()> {
        guard(|| {
            if response.is_null() || status_text.is_null() || status_icon.is_null() {
                return Err(E_INVALIDARG.into());
            }
            let admin = self.admins.borrow().get(*self.admin_sel.borrow() as usize).cloned().unwrap_or_default();
            let password = std::mem::take(&mut *self.password.borrow_mut());
            let fresh = std::mem::replace(&mut *self.typed_fresh.borrow_mut(), true);
            self.show(F_PASSWORD, "");
            let (text, ok) = match self.selected_user() {
                _ if admin.is_empty() || password.is_empty() => {
                    ("Enter the Screen time password.".to_string(), false)
                }
                _ if !fresh => ("Clear the password box and type it again.".to_string(), false),
                None => ("No account to add time for.".to_string(), false),
                Some(username) => {
                    let minutes = ADD_AMOUNTS[*self.add.borrow() as usize];
                    reply_text(ask(SocketRequest::ParentGrant { username, minutes, admin, password }))
                }
            };
            self.refresh();
            let status = self.status.borrow().clone();
            self.show(F_STATUS, &status);
            // This tile never signs anyone in; it only reports what happened.
            unsafe {
                *response = CPGSR_NO_CREDENTIAL_FINISHED;
                *status_text = co_string(&text)?;
                *status_icon = if ok { CPSI_SUCCESS } else { CPSI_ERROR };
            }
            Ok(())
        })
    }

    fn ReportResult(&self, _: NTSTATUS, _: NTSTATUS, _: *mut PWSTR, _: *mut CREDENTIAL_PROVIDER_STATUS_ICON) -> Result<()> {
        Ok(())
    }
}

/// The hourglass on the accent colour, as the tile picture.
fn tile_bitmap() -> Result<HBITMAP> {
    const SIZE: i32 = 192;
    unsafe {
        let module = GetModuleHandleW(windows::core::w!("screentime_credprov.dll"))?;
        let icon = LoadImageW(Some(module.into()), PCWSTR(1 as _), IMAGE_ICON, SIZE, SIZE, LR_DEFAULTCOLOR)?;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        // Fill edge to edge with the icon's blue so the sign-in screen's round crop shows no
        // black corners around the rounded square.
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize);
        pixels.fill(0xff2b59c3);
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, bitmap.into());
        let _ = DrawIconEx(dc, 0, 0, HICON(icon.0), SIZE, SIZE, 0, None, DI_NORMAL);
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        Ok(bitmap)
    }
}

// ---------------------------------------------------------------------------------------------

#[implement(ICredentialProvider)]
struct Provider {
    credential: RefCell<Option<ICredentialProviderCredential>>,
}

impl ICredentialProvider_Impl for Provider_Impl {
    fn SetUsageScenario(&self, scenario: CREDENTIAL_PROVIDER_USAGE_SCENARIO, _flags: u32) -> Result<()> {
        guard(|| {
            if scenario != CPUS_LOGON && scenario != CPUS_UNLOCK_WORKSTATION {
                return Err(E_NOTIMPL.into());
            }
            // No tile unless the service is running and this computer has limited accounts.
            let users = match ask(SocketRequest::ListUsers) {
                Ok(SocketResponse::Users { users }) => users,
                _ => Vec::new(),
            };
            *self.credential.borrow_mut() = (!users.is_empty()).then(|| Credential::new(users).into());
            Ok(())
        })
    }

    fn SetSerialization(&self, _: *const CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Advise(&self, _: Ref<'_, ICredentialProviderEvents>, _: usize) -> Result<()> {
        Ok(())
    }

    fn UnAdvise(&self) -> Result<()> {
        Ok(())
    }

    fn GetFieldDescriptorCount(&self) -> Result<u32> {
        Ok(FIELD_COUNT)
    }

    fn GetFieldDescriptorAt(&self, index: u32) -> Result<*mut CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR> {
        guard(|| {
            if index >= FIELD_COUNT {
                return Err(E_INVALIDARG.into());
            }
            let (kind, label) = field(index);
            unsafe {
                let d = CoTaskMemAlloc(size_of::<CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR>())
                    as *mut CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR;
                if d.is_null() {
                    return Err(E_FAIL.into());
                }
                d.write(CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR {
                    dwFieldID: index,
                    cpft: kind,
                    pszLabel: co_string(&label)?,
                    guidFieldType: GUID::zeroed(),
                });
                Ok(d)
            }
        })
    }

    fn GetCredentialCount(&self, count: *mut u32, default: *mut u32, auto_logon: *mut BOOL) -> Result<()> {
        guard(|| {
            if count.is_null() || default.is_null() || auto_logon.is_null() {
                return Err(E_INVALIDARG.into());
            }
            unsafe {
                *count = self.credential.borrow().is_some() as u32;
                *default = u32::MAX; // CREDENTIAL_PROVIDER_NO_DEFAULT: never preselect this tile
                *auto_logon = false.into();
            }
            Ok(())
        })
    }

    fn GetCredentialAt(&self, index: u32) -> Result<ICredentialProviderCredential> {
        guard(|| match (index, self.credential.borrow().as_ref()) {
            (0, Some(c)) => Ok(c.clone()),
            _ => Err(E_INVALIDARG.into()),
        })
    }
}

// ---------------------------------------------------------------------------------------------

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<'_, IUnknown>, iid: *const GUID, object: *mut *mut c_void) -> Result<()> {
        guard(|| {
            if object.is_null() || iid.is_null() {
                return Err(E_INVALIDARG.into());
            }
            unsafe { *object = std::ptr::null_mut() };
            if outer.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let provider: ICredentialProvider = Provider { credential: RefCell::default() }.into();
            unsafe { provider.query(iid, object).ok() }
        })
    }

    fn LockServer(&self, _: BOOL) -> Result<()> {
        Ok(())
    }
}

/// # Safety
/// Called by COM with valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, iid: *const GUID, object: *mut *mut c_void) -> HRESULT {
    catch_unwind(|| unsafe {
        if clsid.is_null() || iid.is_null() || object.is_null() {
            return E_INVALIDARG;
        }
        *object = std::ptr::null_mut();
        if *clsid != CLSID {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = Factory.into();
        factory.query(iid, object)
    })
    .unwrap_or(E_UNEXPECTED)
}

#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    // LogonUI keeps us loaded; never claim to be unloadable mid-use.
    S_FALSE
}

/// `new` is `old` with one character typed or deleted at the end, as keys do; not a paste or
/// text the box already held.
fn typed_one_key(old: &str, new: &str) -> bool {
    let (o, n) = (old.chars().count(), new.chars().count());
    (n == o + 1 && new.starts_with(old)) || (n + 1 == o && old.starts_with(new))
}

#[cfg(test)]
mod typing_tests {
    use super::typed_one_key;

    #[test]
    fn only_key_by_key_typing_counts() {
        assert!(typed_one_key("", "1") && typed_one_key("12", "123") && typed_one_key("123", "12"));
        // Leftover text being edited, or a whole PIN arriving at once.
        assert!(!typed_one_key("", "1234") && !typed_one_key("", "12345") && !typed_one_key("12", "1x3"));
    }
}
