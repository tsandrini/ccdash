//! The icon in the taskbar's notification area and its menu, over Win32:
//! a window nobody sees owns the icon, and its message loop serves the
//! clicks. Everything here runs on the thread that calls [`run`]; other
//! threads reach it through a [`Remote`].

use std::{
    cell::{Cell, RefCell},
    io, ptr,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{
            NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO,
            NIIF_RESPECT_QUIET_TIME, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
            NIN_SELECT, NINF_KEY, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW,
            DefWindowProcW, DestroyIcon, DestroyMenu, DispatchMessageW, GetMessageW,
            GetSystemMetrics, HICON, KillTimer, LR_DEFAULTCOLOR, MF_GRAYED, MF_SEPARATOR,
            MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
            SM_CXSMICON, SetForegroundWindow, SetMenuDefaultItem, SetProcessDPIAware, SetTimer,
            TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP,
            WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_NULL, WM_QUERYENDSESSION,
            WM_TIMER, WNDCLASSW,
        },
    },
};

use super::icon::{self, Badge};
use crate::win::wide;

/// What the icon shows: its dot and the text under the pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub badge: Badge,
    pub tip: String,
}

/// A line of the icon's menu.
pub enum Item {
    /// Words only: greyed, nothing to choose.
    Label(String),
    /// Something to choose; `id` (not zero) is handed to [`App::chosen`].
    Action {
        id: u32,
        text: String,
    },
    /// As `Action`, and what a click on the icon itself does: in bold.
    Default {
        id: u32,
        text: String,
    },
    Separator,
}

/// A notification from the icon.
pub struct Notice {
    pub title: String,
    pub text: String,
    pub warning: bool,
}

/// What the icon is for. Called on the thread of [`run`], so none of these
/// may take long.
pub trait App {
    fn view(&self) -> View;
    /// A notification to show now; asked at every refresh, so given once.
    fn notice(&self) -> Option<Notice>;
    fn menu(&self) -> Vec<Item>;
    fn chosen(&self, id: u32);
    /// The icon itself was clicked.
    fn clicked(&self);
    /// Windows is about to log the user off or shut down (`true`), or gave
    /// that up again (`false`).
    fn session_ending(&self, ending: bool);
}

/// The way to the loop of [`run`] from other threads; does nothing before
/// the loop runs and after it ended.
#[derive(Clone, Default)]
pub struct Remote(Arc<AtomicIsize>);

impl Remote {
    /// Asks the loop to show the app's view again, and a notice it may have.
    pub fn refresh(&self) {
        self.post(WM_REFRESH);
    }

    /// Asks the loop to end: the icon goes and [`run`] returns.
    pub fn quit(&self) {
        self.post(WM_CLOSE);
    }

    fn post(&self, message: u32) {
        let window = self.0.load(Ordering::Relaxed);
        if window != 0 {
            // SAFETY: plain call; a window that is gone refuses the message
            unsafe { PostMessageW(window as HWND, message, 0, 0) };
        }
    }
}

/// The icon tells its window about clicks with this message.
const WM_ICON: u32 = WM_APP + 1;
/// A [`Remote`] asks for a refresh with this one.
const WM_REFRESH: u32 = WM_APP + 2;
/// The one icon of the window.
const ICON_ID: u32 = 1;
/// The timer that adds the icon again after Explorer did not take it, and
/// its wait in milliseconds.
const ADD_TIMER: usize = 1;
const ADD_AGAIN: u32 = 2000;
/// Two clicks this close are one double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// What the window procedure works with. It is entered again while a menu
/// is open, so nothing here is borrowed across a call into Windows.
struct Ui {
    app: Arc<dyn App>,
    /// The message Explorer broadcasts when its taskbar is (re)made: the
    /// icon has to be added again then.
    taskbar_created: u32,
    shown: RefCell<Option<View>>,
    icon: Cell<HICON>,
    /// Explorer has the icon.
    added: Cell<bool>,
    /// When the icon was last clicked.
    clicked: Cell<Option<Instant>>,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

fn ui() -> Option<Rc<Ui>> {
    UI.with(|ui| ui.borrow().clone())
}

/// Shows the icon and serves it until [`Remote::quit`].
///
/// # Errors
///
/// The window cannot be made.
pub fn run(app: Arc<dyn App>, remote: &Remote) -> io::Result<()> {
    let (class, title, created) = (wide("ccdash-tray"), wide("ccdash"), wide("TaskbarCreated"));
    // SAFETY: plain calls; `created` is NUL-terminated
    let (instance, taskbar_created) = unsafe {
        // NOTE: or the icon is asked for at 16 pixels on every display and stretched
        SetProcessDPIAware();
        (
            GetModuleHandleW(ptr::null()),
            RegisterWindowMessageW(created.as_ptr()),
        )
    };
    UI.with(|ui| {
        *ui.borrow_mut() = Some(Rc::new(Ui {
            app,
            taskbar_created,
            shown: RefCell::new(None),
            icon: Cell::new(ptr::null_mut()),
            added: Cell::new(false),
            clicked: Cell::new(None),
        }));
    });
    let class_info = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    // SAFETY: a whole WNDCLASSW whose strings outlive the window
    if unsafe { RegisterClassW(&raw const class_info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // NOTE: a top-level window, never shown: a message-only one gets no broadcasts, and
    // `TaskbarCreated` is one
    // SAFETY: the class was registered above; NUL-terminated strings
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if window.is_null() {
        return Err(io::Error::last_os_error());
    }
    add_icon(window);
    remote.0.store(window as isize, Ordering::Relaxed);
    // SAFETY: all-zero is a valid MSG
    let mut message: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: `message` is filled by GetMessageW and read by the other two
    while unsafe { GetMessageW(&raw mut message, ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: as above
        unsafe {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    remote.0.store(0, Ordering::Relaxed);
    if let Some(ui) = UI.with(|ui| ui.borrow_mut().take()) {
        destroy_icon(ui.icon.replace(ptr::null_mut()));
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(ui) = ui() else {
        // SAFETY: the arguments as they came
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    };
    match message {
        WM_ICON => {
            // NOTE: version 4 of the icon's messages: the event is the low word of `lparam`,
            // where on the screen the low and high words of `wparam`
            match (lparam as u32) & 0xffff {
                WM_CONTEXTMENU => {
                    let x = i32::from((wparam & 0xffff) as u16 as i16);
                    let y = i32::from(((wparam >> 16) & 0xffff) as u16 as i16);
                    show_menu(&ui, window, x, y);
                }
                event if event == NIN_SELECT || event == (NIN_SELECT | NINF_KEY) => {
                    // NOTE: a double click comes as two clicks, and is one
                    let now = Instant::now();
                    let before = ui.clicked.replace(Some(now));
                    if before.is_none_or(|before| now.duration_since(before) >= DOUBLE_CLICK) {
                        ui.app.clicked();
                    }
                }
                _ => {}
            }
            0
        }
        WM_REFRESH => {
            refresh(&ui, window);
            0
        }
        WM_TIMER if wparam == ADD_TIMER => {
            add_icon(window);
            // NOTE: a notice that came while there was no icon to show it has waited
            refresh(&ui, window);
            0
        }
        // NOTE: Windows asks every window before a log-off or a shutdown, then ends the
        // processes: the app hears of it here, before its collector is ended
        WM_QUERYENDSESSION => {
            ui.app.session_ending(true);
            1
        }
        // NOTE: the answer to the question: the session ends, or does not after all. A forced
        // end may come without the question
        WM_ENDSESSION => {
            ui.app.session_ending(wparam != 0);
            0
        }
        WM_DESTROY => {
            let data = icon_data(window);
            // SAFETY: a whole NOTIFYICONDATAW naming this window's icon; then a plain call
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &raw const data);
                PostQuitMessage(0);
            }
            0
        }
        created if created == ui.taskbar_created && created != 0 => {
            add_icon(window);
            refresh(&ui, window);
            0
        }
        // SAFETY: the arguments as they came
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// `text` in a fixed field of a `NOTIFYICONDATAW`, cut to fit, terminated.
fn fill(field: &mut [u16], text: &str) {
    let room = field.len() - 1;
    let mut end = 0;
    for (slot, unit) in field[..room].iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
        end += 1;
    }
    // NOTE: not half of a pair of surrogates at the cut
    if end == room && (0xd800..0xdc00).contains(&field[end - 1]) {
        end -= 1;
    }
    field[end] = 0;
}

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    // SAFETY: all-zero is a valid NOTIFYICONDATAW
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = window;
    data.uID = ICON_ID;
    data
}

fn make_icon(badge: Badge) -> HICON {
    // SAFETY: plain call
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
    let bits = icon::resource(size as usize, badge);
    // SAFETY: `bits` is a whole icon resource of `size` pixels a side; 0x30000 is its format
    unsafe {
        CreateIconFromResourceEx(
            bits.as_ptr(),
            bits.len() as u32,
            1,
            0x0003_0000,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
    }
}

fn destroy_icon(icon: HICON) {
    if !icon.is_null() {
        // SAFETY: an icon made by `make_icon` that nothing shows any more
        unsafe { DestroyIcon(icon) };
    }
}

/// Puts the icon into the notification area: at the start, whenever
/// Explorer made its taskbar anew (`TaskbarCreated`), and again on a timer
/// for as long as Explorer does not take it, which a busy one at logon may
/// not, with no broadcast to follow.
fn add_icon(window: HWND) {
    let Some(ui) = ui() else { return };
    let view = ui.app.view();
    let icon = make_icon(view.badge);
    let mut data = icon_data(window);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = WM_ICON;
    data.hIcon = icon;
    fill(&mut data.szTip, &view.tip);
    // SAFETY: a whole NOTIFYICONDATAW; the icon lives until it is replaced; a live window
    let added = unsafe {
        // NOTE: Explorer may still hold the icon of an earlier add
        Shell_NotifyIconW(NIM_DELETE, &raw const data);
        let added = Shell_NotifyIconW(NIM_ADD, &raw const data) != 0 && {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &raw const data) != 0
        };
        if added {
            KillTimer(window, ADD_TIMER);
        } else {
            // NOTE: not an icon of the old version, whose clicks come as other messages
            Shell_NotifyIconW(NIM_DELETE, &raw const data);
            SetTimer(window, ADD_TIMER, ADD_AGAIN, None);
        }
        added
    };
    ui.added.set(added);
    destroy_icon(ui.icon.replace(icon));
    *ui.shown.borrow_mut() = Some(view);
}

/// Shows the app's view when it changed, and a notice when it has one.
fn refresh(ui: &Ui, window: HWND) {
    // NOTE: no icon yet: the timer adds it with the view of then, and a notice waits for it
    if !ui.added.get() {
        return;
    }
    let view = ui.app.view();
    let shown = ui.shown.borrow().clone();
    if shown.as_ref() != Some(&view) {
        let mut data = icon_data(window);
        data.uFlags = NIF_TIP | NIF_SHOWTIP;
        fill(&mut data.szTip, &view.tip);
        let mut old = ptr::null_mut();
        if shown.is_none_or(|shown| shown.badge != view.badge) {
            data.uFlags |= NIF_ICON;
            data.hIcon = make_icon(view.badge);
            old = ui.icon.replace(data.hIcon);
        }
        // SAFETY: a whole NOTIFYICONDATAW; the new icon lives until it is replaced
        let modified = unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) } != 0;
        destroy_icon(old);
        *ui.shown.borrow_mut() = Some(view);
        if !modified {
            // NOTE: Explorer no longer has the icon, and did not say so
            add_icon(window);
            if !ui.added.get() {
                return;
            }
        }
    }
    if let Some(notice) = ui.app.notice() {
        let mut data = icon_data(window);
        data.uFlags = NIF_INFO;
        fill(&mut data.szInfoTitle, &notice.title);
        fill(&mut data.szInfo, &notice.text);
        let kind = if notice.warning {
            NIIF_WARNING
        } else {
            NIIF_INFO
        };
        data.dwInfoFlags = kind | NIIF_RESPECT_QUIET_TIME;
        // SAFETY: a whole NOTIFYICONDATAW
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) };
    }
}

/// The app's menu at a point of the screen; what was chosen goes to the app.
fn show_menu(ui: &Ui, window: HWND, x: i32, y: i32) {
    let items = ui.app.menu();
    // SAFETY: plain call; the menu is destroyed below
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    for item in &items {
        let (flags, id, text) = match item {
            Item::Label(text) => (MF_STRING | MF_GRAYED, 0, Some(text)),
            Item::Action { id, text } | Item::Default { id, text } => (MF_STRING, *id, Some(text)),
            Item::Separator => (MF_SEPARATOR, 0, None),
        };
        let text = text.map(|text| wide(text));
        // SAFETY: a live menu; the text is NUL-terminated and copied by the call
        unsafe {
            AppendMenuW(
                menu,
                flags,
                id as usize,
                text.as_ref().map_or(ptr::null(), |text| text.as_ptr()),
            );
            if matches!(item, Item::Default { .. }) {
                SetMenuDefaultItem(menu, id, 0);
            }
        }
    }
    // SAFETY: a live window and menu. The window comes to the front so that a click
    // elsewhere closes the menu, and the empty message after it lets the menu go for good
    // (both as Microsoft's notes on TrackPopupMenu say)
    let chosen = unsafe {
        SetForegroundWindow(window);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            x,
            y,
            0,
            window,
            ptr::null(),
        );
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
        chosen
    };
    if let Ok(id @ 1..) = u32::try_from(chosen) {
        ui.app.chosen(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_fits_its_field_whole_or_cut_and_ends_with_a_terminator() {
        let mut field = [0xffffu16; 8];
        fill(&mut field, "ccdash");
        assert_eq!(String::from_utf16_lossy(&field[..6]), "ccdash");
        assert_eq!(field[6], 0);
        fill(&mut field, "connected to the hub");
        assert_eq!(String::from_utf16_lossy(&field[..7]), "connect");
        assert_eq!(field[7], 0);
        // NOTE: a character of two units that the cut would halve goes whole
        fill(&mut field, "123456\u{1f600}");
        assert_eq!(String::from_utf16_lossy(&field[..6]), "123456");
        assert_eq!(field[6], 0);
    }
}
