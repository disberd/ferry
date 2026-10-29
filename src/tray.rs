// The tray icon and the hidden window of `ferry run`. The window is a top-level
// window, because a message-only window gets no broadcast messages. It gets the
// tray callbacks, the wake from sleep and `TaskbarCreated`. It belongs to the
// main thread, and the message loop of that thread (`hotkey`) delivers its messages.
// The icon follows the `Link`: each state change posts one message to the window.

use crate::lifecycle::State;
use crate::link::{status_text, Icon, Link};
use crate::windows::{log, wide};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, OnceLock};

type H = *mut c_void;

#[repr(C)]
struct WndClass {
    style: u32,
    proc_: unsafe extern "system" fn(H, u32, usize, isize) -> isize,
    class_extra: i32,
    window_extra: i32,
    instance: H,
    icon: H,
    cursor: H,
    brush: H,
    menu: *const u16,
    class: *const u16,
}

// NOTIFYICONDATAW.
#[repr(C)]
struct Nid {
    size: u32,
    wnd: H,
    id: u32,
    flags: u32,
    callback: u32,
    icon: H,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: [u8; 16],
    balloon_icon: H,
}

#[repr(C)]
struct IconInfo {
    is_icon: i32,
    hot_x: u32,
    hot_y: u32,
    mask: H,
    color: H,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(c: *const WndClass) -> u16;
    fn CreateWindowExW(ex: u32, class: *const u16, name: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: H, menu: H, instance: H, param: *mut c_void) -> H;
    fn DefWindowProcW(h: H, m: u32, w: usize, l: isize) -> isize;
    fn PostMessageW(h: H, m: u32, w: usize, l: isize) -> i32;
    fn RegisterWindowMessageW(name: *const u16) -> u32;
    fn CreateIconIndirect(i: *const IconInfo) -> H;
    fn CreatePopupMenu() -> H;
    fn AppendMenuW(menu: H, flags: u32, id: usize, text: *const u16) -> i32;
    fn TrackPopupMenu(menu: H, flags: u32, x: i32, y: i32, reserved: i32, wnd: H, rect: *const c_void) -> i32;
    fn DestroyMenu(menu: H) -> i32;
    fn GetCursorPos(p: *mut [i32; 2]) -> i32;
    fn SetForegroundWindow(h: H) -> i32;
    fn GetSystemMetrics(i: i32) -> i32;
}
#[link(name = "gdi32")]
extern "system" {
    fn CreateBitmap(w: i32, h: i32, planes: u32, bits: u32, data: *const c_void) -> H;
    fn DeleteObject(h: H) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> H;
}
#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconW(op: u32, nid: *const Nid) -> i32;
    fn ShellExecuteW(wnd: H, op: *const u16, file: *const u16, params: *const u16, dir: *const u16, show: i32) -> isize;
}

const WM_CALLBACK: u32 = 0x8001; // WM_APP + 1: the tray callback
const WM_CHANGED: u32 = 0x8002; // WM_APP + 2: the `Link` changed
const WM_LBUTTONUP: usize = 0x202;
const WM_RBUTTONUP: usize = 0x205;
const WM_POWERBROADCAST: u32 = 0x218;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIF_MESSAGE_ICON_TIP: u32 = 1 | 2 | 4;
const MF_GRAYED: u32 = 1;
const MF_SEPARATOR: u32 = 0x800;
const TPM_RIGHTBUTTON: u32 = 2;
const TPM_RETURNCMD: u32 = 0x100;
const SM_CXSMICON: i32 = 49;
const OPEN_LOG: i32 = 1;
const QUIT: i32 = 2;

struct Ctx {
    link: Arc<Link>,
    state: State,
    log: String,
    // The `HICON`s of `Icon::Normal`, `Grey` and `Alert`, as numbers.
    icons: [usize; 3],
    // The id of the `TaskbarCreated` message.
    created: u32,
}

// The window handle as a number, 0 before the window exists.
static WND: AtomicUsize = AtomicUsize::new(0);
static CTX: OnceLock<Ctx> = OnceLock::new();

// The colour of the pixel at (u, v), both in 0..1, or `None` outside the icon.
// A rounded square with a white F, and a red dot for `Alert`.
fn shade(u: f32, v: f32, icon: Icon) -> Option<[u32; 3]> {
    if icon == Icon::Alert && (u - 0.72).powi(2) + (v - 0.72).powi(2) < 0.25 * 0.25 {
        return Some([229, 57, 53]);
    }
    let (qx, qy) = ((u - 0.5).abs() - 0.26, (v - 0.5).abs() - 0.26);
    if (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) > 0.2 {
        return None;
    }
    let bar = |x0: f32, x1: f32, y0: f32, y1: f32| u >= x0 && u < x1 && v >= y0 && v < y1;
    if bar(0.32, 0.46, 0.22, 0.78) || bar(0.32, 0.72, 0.22, 0.36) || bar(0.32, 0.62, 0.46, 0.58) {
        return Some([255, 255, 255]);
    }
    Some(if icon == Icon::Grey { [138, 138, 138] } else { [43, 108, 176] })
}

// One `n` by `n` icon: 2 by 2 samples for each pixel, so the edges are smooth.
fn make_icon(n: i32, icon: Icon) -> H {
    let side = n as usize;
    let mut bgra = vec![0u8; side * side * 4];
    for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
        let (x, y) = ((i % side) as f32, (i / side) as f32);
        let (mut sum, mut hit) = ([0u32; 3], 0u32);
        for s in 0..4 {
            let (u, v) = ((x + 0.25 + 0.5 * (s % 2) as f32) / n as f32, (y + 0.25 + 0.5 * (s / 2) as f32) / n as f32);
            if let Some(c) = shade(u, v, icon) {
                (0..3).for_each(|k| sum[k] += c[k]);
                hit += 1;
            }
        }
        // With no hit, the sums and the alpha are 0 and the pixel stays clear.
        let d = hit.max(1);
        px.copy_from_slice(&[(sum[2] / d) as u8, (sum[1] / d) as u8, (sum[0] / d) as u8, (hit * 255 / 4) as u8]);
    }
    // The mask stays zero: the alpha channel of the colour bitmap does the shaping.
    let zeros = vec![0u8; side * side];
    unsafe {
        let info = IconInfo { is_icon: 1, hot_x: 0, hot_y: 0, mask: CreateBitmap(n, n, 1, 1, zeros.as_ptr().cast()), color: CreateBitmap(n, n, 1, 32, bgra.as_ptr().cast()) };
        let h = CreateIconIndirect(&info);
        DeleteObject(info.mask);
        DeleteObject(info.color);
        h
    }
}

fn nid(flags: u32) -> Nid {
    let mut n: Nid = unsafe { std::mem::zeroed() };
    n.size = std::mem::size_of::<Nid>() as u32;
    n.wnd = WND.load(SeqCst) as H;
    n.id = 1;
    n.flags = flags;
    n
}

// Shows the icon and the hover line of the `Link`. `NIM_MODIFY` fails when the
// icon is not there yet (the start, or a new taskbar), so then it adds the icon.
fn show(c: &Ctx) {
    let (icon, tip) = c.link.view().map_or((Icon::Grey, "ferry · starting".to_string()), |(_, t)| (t.icon, t.tip));
    let mut n = nid(NIF_MESSAGE_ICON_TIP);
    n.callback = WM_CALLBACK;
    n.icon = c.icons[icon as usize] as H;
    // The tip has 127 units at most, so the last unit stays 0.
    n.tip.iter_mut().zip(tip.encode_utf16()).for_each(|(d, u)| *d = u);
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) == 0 && Shell_NotifyIconW(NIM_ADD, &n) == 0 } {
        log(&format!("FAIL: tray icon, error {}", crate::windows::last_error()));
    }
}

fn open_log(c: &Ctx) {
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), wide("open").as_ptr(), wide(&c.log).as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
    if r <= 32 {
        log(&format!("FAIL: open log, code {r}"));
    }
}

// No `BYE`: the Remote sees the channel end. The Job object ends `ssh.exe`
// with this process.
fn quit() -> ! {
    log("quit from the tray menu");
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid(0)) };
    std::process::exit(0)
}

// The status lines as greyed items, then `Open log` and `Quit`.
fn menu(h: H, c: &Ctx) {
    let view = c.link.view();
    let (lines, _) = status_text(crate::frame::VERSION, Some(&c.state), true, view.as_ref().map(|v| &v.0), &c.log);
    let choice = unsafe {
        let m = CreatePopupMenu();
        for l in lines.lines() {
            // `&` marks a shortcut key in a menu text.
            AppendMenuW(m, MF_GRAYED, 0, wide(&l.replace('&', "&&")).as_ptr());
        }
        AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(m, 0, OPEN_LOG as usize, wide("Open log").as_ptr());
        AppendMenuW(m, 0, QUIT as usize, wide("Quit").as_ptr());
        let mut p = [0i32; 2];
        GetCursorPos(&mut p);
        // Without this the menu stays open when the user clicks elsewhere.
        SetForegroundWindow(h);
        let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p[0], p[1], 0, h, std::ptr::null());
        PostMessageW(h, 0, 0, 0); // WM_NULL ends the menu loop of the foreground rule
        DestroyMenu(m);
        id
    };
    match choice {
        OPEN_LOG => open_log(c),
        QUIT => quit(),
        _ => {}
    }
}

unsafe extern "system" fn window_proc(h: H, m: u32, w: usize, l: isize) -> isize {
    let Some(c) = CTX.get() else { return DefWindowProcW(h, m, w, l) };
    match m {
        WM_CALLBACK if matches!(l as usize & 0xffff, WM_LBUTTONUP | WM_RBUTTONUP) => menu(h, c),
        WM_CHANGED => show(c),
        WM_POWERBROADCAST if w == PBT_APMRESUMEAUTOMATIC => {
            log("wake from sleep: the next wait is 5 s");
            c.link.reset();
            return 1;
        }
        _ if m == c.created => show(c),
        _ => return DefWindowProcW(h, m, w, l),
    }
    0
}

// Makes the hidden window and the icon. Call it on the thread that runs the
// message loop. A failure only costs the icon, so it goes to the log.
pub fn create(link: Arc<Link>, state: State, log_path: String) {
    let n = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let icons = [Icon::Normal, Icon::Grey, Icon::Alert].map(|i| make_icon(n, i) as usize);
    let created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    let c = CTX.get_or_init(|| Ctx { link, state, log: log_path, icons, created });
    let class = wide("ferry");
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = WndClass { style: 0, proc_: window_proc, class_extra: 0, window_extra: 0, instance, icon: std::ptr::null_mut(), cursor: std::ptr::null_mut(), brush: std::ptr::null_mut(), menu: std::ptr::null(), class: class.as_ptr() };
    let h = unsafe {
        RegisterClassW(&wc);
        CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null_mut())
    };
    if h.is_null() {
        return log(&format!("FAIL: hidden window, error {}", crate::windows::last_error()));
    }
    WND.store(h as usize, SeqCst);
    show(c);
}

// Tells the window that the `Link` changed. Any thread may call it, also with the
// lock of the `Link` held: it only posts a message.
pub fn changed() {
    let w = WND.load(SeqCst);
    if w != 0 {
        unsafe { PostMessageW(w as H, WM_CHANGED, 0, 0) };
    }
}
