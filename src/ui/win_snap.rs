//! Windows: dragging the window to the top of the screen maximizes it, as with any
//! Windows app. GTK moves its windows itself rather than through Windows' own window
//! drag, so Windows' snapping never sees it.

use std::cell::{Cell, RefCell};

use gtk::{gdk, glib, prelude::*};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
    UI::{
        Input::KeyboardAndMouse::GetCapture,
        Shell::{DefSubclassProc, SetWindowSubclass},
        WindowsAndMessaging::{
            GetCursorPos, GetWindowPlacement, GetWindowRect, SW_SHOWNORMAL, SWP_NOMOVE, SWP_NOSIZE, SetWindowPlacement,
            WINDOWPLACEMENT, WINDOWPOS, WM_CAPTURECHANGED, WM_LBUTTONUP, WM_WINDOWPOSCHANGING,
        },
    },
};

unsafe extern "C" {
    fn gdk_win32_surface_get_handle(surface: *mut gdk::ffi::GdkSurface) -> HWND;
}

/// Ours among the window's subclasses.
const SUBCLASS_ID: usize = 0x5041_534e;
/// How close to the top of the screen (pixels) the pointer must be when let go.
const TOP_EDGE: i32 = 2;

thread_local! {
    /// Where the window was before the drag now under way, to go back to when restored.
    static BEFORE_DRAG: Cell<Option<WINDOWPLACEMENT>> = const { Cell::new(None) };
    static WINDOW: RefCell<glib::WeakRef<adw::ApplicationWindow>> = RefCell::default();
}

pub fn install(window: &adw::ApplicationWindow) {
    WINDOW.with(|w| w.borrow().set(Some(window)));
    window.connect_realize(|w| {
        let Some(surface) = w.surface() else { return };
        // Safety: a live GDK surface on the UI thread, which is where its window runs.
        unsafe {
            let hwnd = gdk_win32_surface_get_handle(surface.as_ptr());
            SetWindowSubclass(hwnd, Some(snap_proc), SUBCLASS_ID, 0);
        }
    });
}

unsafe extern "system" fn snap_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_WINDOWPOSCHANGING => {
            // A drag moves the window at its current size, with the pointer captured.
            let pos = unsafe { &*(lparam as *const WINDOWPOS) };
            if pos.flags & SWP_NOMOVE == 0 && unsafe { GetCapture() } == hwnd && same_size(hwnd, pos) {
                BEFORE_DRAG.with(|b| {
                    if b.get().is_none() {
                        let mut p: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
                        p.length = size_of::<WINDOWPLACEMENT>() as u32;
                        if unsafe { GetWindowPlacement(hwnd, &mut p) } != 0 {
                            b.set(Some(p));
                        }
                    }
                });
            }
        }
        WM_LBUTTONUP => {
            if let Some(mut p) = BEFORE_DRAG.with(Cell::take) {
                if pointer_at_top_of_screen() {
                    p.showCmd = SW_SHOWNORMAL as u32;
                    // Once GTK has finished its drag. Back where the drag began first, so
                    // that's where restoring returns to; GTK does the maximizing, since
                    // it has to account for its window shadow.
                    glib::idle_add_local_once(move || {
                        unsafe { SetWindowPlacement(hwnd, &p) };
                        if let Some(w) = WINDOW.with(|w| w.borrow().upgrade()) {
                            w.maximize();
                        }
                    });
                }
            }
        }
        WM_CAPTURECHANGED => BEFORE_DRAG.with(|b| b.set(None)),
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

fn same_size(hwnd: HWND, pos: &WINDOWPOS) -> bool {
    if pos.flags & SWP_NOSIZE != 0 {
        return true;
    }
    let mut r: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetWindowRect(hwnd, &mut r) != 0 && r.right - r.left == pos.cx && r.bottom - r.top == pos.cy }
}

fn pointer_at_top_of_screen() -> bool {
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return false;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<MONITORINFO>() as u32;
    let monitor = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    unsafe { GetMonitorInfoW(monitor, &mut info) != 0 && pt.y <= info.rcMonitor.top + TOP_EDGE }
}
