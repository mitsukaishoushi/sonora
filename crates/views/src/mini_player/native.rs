//! Windows companion-window operations missing from GPUI's portable window interface.
//! Run these in a foreground task, outside a borrowed GPUI window update.

use gpui::{Bounds, Pixels, Window, px};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetClientRect, GetCursorPos, GetWindowRect, HMENU, HWND_NOTOPMOST,
    HWND_TOPMOST, IsWindow, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MENU_ITEM_FLAGS,
    PostMessageW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
    SetWindowPos, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_NULL,
};
use windows::core::PCWSTR;

/// Localized labels prepared before opening the menu.
pub(crate) struct Labels {
    pub position: String,
    pub size: String,
    pub on_top: String,
    pub restore: String,
    pub alternative: Option<String>,
    pub positions: Vec<(u32, String, bool)>,
    pub sizes: Vec<(u32, String, bool)>,
}

/// Owns unattached menus, transferring a child's ownership only after a successful append.
struct Menu(Option<HMENU>);

impl Menu {
    fn new() -> windows::core::Result<Self> {
        unsafe { CreatePopupMenu().map(|menu| Self(Some(menu))) }
    }

    fn handle(&self) -> HMENU { self.0.expect("owned menu") }

    fn item(&self, id: u32, label: &str, checked: bool) -> windows::core::Result<()> {
        let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
        let check = if checked { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
        unsafe { AppendMenuW(self.handle(), MF_STRING | check, id as usize, PCWSTR(text.as_ptr())) }
    }

    fn submenu(&self, label: &str, mut child: Self) -> windows::core::Result<()> {
        let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
        unsafe { AppendMenuW(self.handle(), MF_POPUP, child.handle().0 as usize, PCWSTR(text.as_ptr()))?; }
        child.0 = None;
        Ok(())
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        if let Some(menu) = self.0 { unsafe { let _ = DestroyMenu(menu); } }
    }
}

/// Copies the OS handle without extending a GPUI borrow into a native message loop.
pub(crate) fn handle(window: &Window) -> Option<isize> {
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// Applies client-area bounds, accounting for the invisible Windows frame on every move.
pub(crate) fn apply(owner: isize, bounds: Option<Bounds<Pixels>>, scale: f32, on_top: Option<bool>) {
    let hwnd = HWND(owner as *mut _);
    // A queued request may outlive a window closed in the meantime.
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() { return; }
    let mut flags = SWP_NOACTIVATE;
    let after = match on_top {
        Some(true) => Some(HWND_TOPMOST),
        Some(false) => Some(HWND_NOTOPMOST),
        None => { flags |= SWP_NOZORDER; None }
    };
    let (x, y, width, height) = match bounds {
        Some(bounds) => match outer_frame(hwnd, bounds, scale) {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("mini player: cannot measure window frame: {error}");
                return;
            }
        },
        None => { flags |= SWP_NOMOVE | SWP_NOSIZE; (0, 0, 0, 0) }
    };
    if let Err(error) = unsafe { SetWindowPos(hwnd, after, x, y, width, height, flags) } {
        log::warn!("mini player: cannot update window: {error}");
    }
}

/// SetWindowPos sizes the outer frame; artwork and saved coordinates describe its client area.
/// Measuring both rectangles also handles asymmetric borders and the current monitor's DPI.
fn outer_frame(hwnd: HWND, bounds: Bounds<Pixels>, scale: f32) -> windows::core::Result<(i32, i32, i32, i32)> {
    let mut outer = RECT::default();
    let mut client = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        GetWindowRect(hwnd, &mut outer)?;
        GetClientRect(hwnd, &mut client)?;
        ClientToScreen(hwnd, &mut origin).ok()?;
    }
    let border_width = (outer.right - outer.left) - (client.right - client.left);
    let border_height = (outer.bottom - outer.top) - (client.bottom - client.top);
    Ok((
        (bounds.origin.x / px(1.) * scale).round() as i32 - (origin.x - outer.left),
        (bounds.origin.y / px(1.) * scale).round() as i32 - (origin.y - outer.top),
        (bounds.size.width / px(1.) * scale).round() as i32 + border_width,
        (bounds.size.height / px(1.) * scale).round() as i32 + border_height,
    ))
}

/// Native submenus can extend beyond even the smallest artwork window and stay on screen.
pub(crate) fn popup(owner: isize, labels: Labels, on_top: bool) -> u32 {
    match popup_inner(owner, labels, on_top) {
        Ok(command) => command,
        Err(error) => { log::warn!("mini player: cannot open menu: {error}"); 0 }
    }
}

fn popup_inner(owner: isize, labels: Labels, on_top: bool) -> windows::core::Result<u32> {
    let hwnd = HWND(owner as *mut _);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() { return Ok(0); }
    let root = Menu::new()?;
    let positions = Menu::new()?;
    for (id, label, checked) in labels.positions { positions.item(id, &label, checked)?; }
    root.submenu(&labels.position, positions)?;
    let sizes = Menu::new()?;
    for (id, label, checked) in labels.sizes { sizes.item(id, &label, checked)?; }
    root.submenu(&labels.size, sizes)?;
    root.item(21, &labels.on_top, on_top)?;
    if let Some(alternative) = labels.alternative {
        root.item(23, &alternative, false)?;
    }
    unsafe { AppendMenuW(root.handle(), MF_SEPARATOR, 0, PCWSTR::null())?; }
    root.item(22, &labels.restore, false)?;
    let mut cursor = POINT::default();
    unsafe {
        GetCursorPos(&mut cursor)?;
        let _ = SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(root.handle(), TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            cursor.x, cursor.y, None, hwnd, None).0 as u32;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        Ok(command)
    }
}
