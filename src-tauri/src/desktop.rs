//! Pin the gadget to the desktop layer, Vista-gadget style: occluded by
//! foreground windows, visible when the desktop is, and still interactive.
//!
//! Two techniques, because the desktop window tree changed across builds:
//!
//! Classic (pre-24H2): ask Progman to spawn the WorkerW behind the desktop
//! icons (undocumented 0x052C) and SetParent onto it — the Rainmeter /
//! Wallpaper Engine approach. Detectable signature: SHELLDLL_DefView lives
//! inside a WorkerW.
//!
//! Win11 24H2+: DefView never leaves Progman and no wallpaper WorkerW spawns.
//! Any window parented into that region — or pushed to absolute HWND_BOTTOM,
//! which now lands *beneath* Progman — stays visible (wallpaper is composited
//! behind everything) but the desktop icon listview wins all hit-testing:
//! visible, unclickable. So on this layout the gadget is kept a top-level
//! window pinned in the z-order slot directly *above* Progman instead.

use std::sync::atomic::{AtomicIsize, Ordering};
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GetClassNameW, GetParent, GetWindowLongPtrW,
    SendMessageTimeoutW, SetParent, SetWindowLongPtrW, SetWindowPos, GWLP_HWNDPARENT,
    GWL_EXSTYLE, HWND_BOTTOM, SMTO_NORMAL, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

static FOUND_WORKERW: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn enum_cb(hwnd: HWND, _l: LPARAM) -> i32 {
    // Classic layout only: DefView hosted by a WorkerW (NOT by Progman —
    // that's the 24H2 signature, where this technique kills mouse input).
    if class_of(hwnd) != "WorkerW" {
        return 1;
    }
    let defview = FindWindowExW(
        hwnd,
        std::ptr::null_mut(),
        wide("SHELLDLL_DefView").as_ptr(),
        std::ptr::null(),
    );
    if !defview.is_null() {
        // The wallpaper WorkerW is the next top-level WorkerW sibling.
        let worker = FindWindowExW(
            std::ptr::null_mut(),
            hwnd,
            wide("WorkerW").as_ptr(),
            std::ptr::null(),
        );
        if !worker.is_null() {
            FOUND_WORKERW.store(worker as isize, Ordering::SeqCst);
        }
    }
    1
}

fn find_classic_workerw() -> HWND {
    unsafe {
        let progman = FindWindowW(wide("Progman").as_ptr(), std::ptr::null());
        if progman.is_null() {
            return std::ptr::null_mut();
        }
        let mut out: usize = 0;
        // Both known variants of the WorkerW-spawn message.
        SendMessageTimeoutW(progman, 0x052C, 0xD, 0x1, SMTO_NORMAL, 1000, &mut out);
        SendMessageTimeoutW(progman, 0x052C, 0, 0, SMTO_NORMAL, 1000, &mut out);

        FOUND_WORKERW.store(0, Ordering::SeqCst);
        EnumWindows(Some(enum_cb), 0);
        FOUND_WORKERW.load(Ordering::SeqCst) as HWND
    }
}

fn apply_noactivate_toolwindow(hwnd: HWND) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ex | WS_EX_TOOLWINDOW as isize | WS_EX_NOACTIVATE as isize,
        );
    }
}

/// Returns the mode that actually took effect:
/// "workerw" | "workerw-progman" | "bottom" | "normal".
pub fn attach_to_desktop(raw_hwnd: isize, mode: &str) -> String {
    let hwnd = raw_hwnd as HWND;
    if mode == "normal" {
        return "normal".into();
    }
    if mode != "bottom" {
        let worker = find_classic_workerw();
        if !worker.is_null() {
            unsafe {
                SetParent(hwnd, worker);
                if GetParent(hwnd) == worker {
                    return "workerw".into();
                }
            }
        }
        // 24H2+: parenting onto Progman puts the gadget behind the icon
        // listview, which eats mouse input — look-but-don't-touch. Opt-in
        // only, never part of "auto".
        if mode == "workerw" {
            unsafe {
                let progman = FindWindowW(wide("Progman").as_ptr(), std::ptr::null());
                if !progman.is_null() {
                    SetParent(hwnd, progman);
                    if GetParent(hwnd) == progman {
                        return "workerw-progman".into();
                    }
                }
            }
        }
    }
    // Fallback (and explicit "bottom" mode): no-activate tool window OWNED by
    // Progman. Windows keeps an owned window directly above its owner in the
    // z-order — the desktop-gadget slot — with no pinning race. A plain
    // HWND_BOTTOM would sink *beneath* Progman on 24H2 (visible via DWM
    // composition, but the icon listview wins every hit-test).
    apply_noactivate_toolwindow(hwnd);
    unsafe {
        let progman = FindWindowW(wide("Progman").as_ptr(), std::ptr::null());
        if !progman.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, progman as isize);
        }
    }
    pin_bottom(raw_hwnd);
    "bottom".into()
}

/// Re-assert the desktop-layer z-position; called periodically in "bottom"
/// mode. With Progman as owner, HWND_BOTTOM floors at "directly above the
/// desktop" instead of sinking beneath it.
pub fn pin_bottom(raw_hwnd: isize) {
    unsafe {
        SetWindowPos(
            raw_hwnd as HWND,
            HWND_BOTTOM,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// WS_EX_NOACTIVATE blocks keyboard focus, which breaks typing into the
/// settings panel — the UI toggles it off while settings are open.
pub fn set_activatable(raw_hwnd: isize, on: bool) {
    unsafe {
        let hwnd = raw_hwnd as HWND;
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new = if on {
            ex & !(WS_EX_NOACTIVATE as isize)
        } else {
            ex | WS_EX_NOACTIVATE as isize
        };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new);
    }
}
