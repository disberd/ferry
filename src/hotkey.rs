// Workstation side of the hotkey flow. The hotkey (Ctrl+Alt+V by default) is registered only while
// the foreground exe is on the `--apps` list: a foreground event hook and a check at start feed the
// exe name to `active`, which decides. On the hotkey it reads the
// clipboard: files (CF_HDROP) first, else the PNG image, else an error sound. `exchange::press`
// sends them to `ferry serve` over the channel; `serve` types the Remote paths into the focused
// herdr pane, or the paths go to the clipboard. The press runs on its own thread, so the
// message loop (and the tray icon) stays live during a transfer. Every press is logged; a focus
// change is not.

use std::ffi::c_void;
use crate::active::{Act, Active, Seen};
use crate::exchange::{self, Item};
use crate::lifecycle::Opts;
use crate::link::Link;
use crate::sounds::{play, Sound};
use crate::windows::{last_error, log, wide};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

type H = *mut c_void;

#[repr(C)]
struct Msg {
    hwnd: H,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: [i32; 2],
    private: u32,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterHotKey(hwnd: H, id: i32, mods: u32, vk: u32) -> i32;
    fn UnregisterHotKey(hwnd: H, id: i32) -> i32;
    fn SetWinEventHook(min: u32, max: u32, module: H, proc_: unsafe extern "system" fn(H, u32, H, i32, i32, u32, u32), pid: u32, tid: u32, flags: u32) -> H;
    fn GetWindowThreadProcessId(h: H, pid: *mut u32) -> u32;
    fn GetMessageW(msg: *mut Msg, hwnd: H, min: u32, max: u32) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn OpenClipboard(hwnd: H) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn EnumClipboardFormats(fmt: u32) -> u32;
    fn GetClipboardFormatNameW(fmt: u32, buf: *mut u16, n: i32) -> i32;
    fn RegisterClipboardFormatW(name: *const u16) -> u32;
    fn GetClipboardData(fmt: u32) -> H;
    fn SetClipboardData(fmt: u32, h: H) -> H;
    fn GetForegroundWindow() -> H;
    fn GetWindowTextW(h: H, buf: *mut u16, n: i32) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GlobalLock(h: H) -> *mut c_void;
    fn GlobalUnlock(h: H) -> i32;
    fn GlobalSize(h: H) -> usize;
    fn GlobalAlloc(flags: u32, n: usize) -> H;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> H;
    fn QueryFullProcessImageNameW(h: H, flags: u32, buf: *mut u16, n: *mut u32) -> i32;
    fn CloseHandle(h: H) -> i32;
}
#[link(name = "shell32")]
extern "system" {
    fn DragQueryFileW(h: H, i: u32, buf: *mut u16, n: u32) -> u32;
}
const WM_HOTKEY: u32 = 0x0312;
const MOD_NOREPEAT: u32 = 0x4000;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const EVENT_SYSTEM_FOREGROUND: u32 = 3;
const WINEVENT_OUTOFCONTEXT: u32 = 0;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const UNKNOWN_EXE: &str = "unknown";

fn format_name(f: u32) -> String {
    let known = [(1, "TEXT"), (2, "BITMAP"), (3, "METAFILEPICT"), (7, "OEMTEXT"), (8, "DIB"), (13, "UNICODETEXT"),
                 (14, "ENHMETAFILE"), (15, "HDROP"), (16, "LOCALE"), (17, "DIBV5")];
    if let Some((_, n)) = known.iter().find(|(k, _)| *k == f) {
        return n.to_string();
    }
    let mut buf = [0u16; 128];
    let n = unsafe { GetClipboardFormatNameW(f, buf.as_mut_ptr(), 128) };
    if n > 0 { String::from_utf16_lossy(&buf[..n as usize]) } else { format!("#{f}") }
}

enum Clip {
    Files(Vec<PathBuf>),
    Png(Vec<u8>),
    Nothing,
}

fn open_clipboard() -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

fn read_clipboard() -> Result<(Clip, Vec<String>), String> {
    if !open_clipboard() {
        return Err(format!("OpenClipboard failed, error {}", last_error()));
    }
    let png_fmt = unsafe { RegisterClipboardFormatW(wide("PNG").as_ptr()) };
    let mut formats = Vec::new();
    let mut f = 0;
    loop {
        f = unsafe { EnumClipboardFormats(f) };
        if f == 0 { break; }
        formats.push(f);
    }
    let names = formats.iter().map(|&f| format_name(f)).collect();
    let clip = unsafe {
        if formats.contains(&CF_HDROP) {
            let h = GetClipboardData(CF_HDROP);
            let n = DragQueryFileW(h, u32::MAX, std::ptr::null_mut(), 0);
            let files = (0..n)
                .map(|i| {
                    let mut buf = [0u16; 1024];
                    let len = DragQueryFileW(h, i, buf.as_mut_ptr(), 1024);
                    PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]))
                })
                .collect();
            Clip::Files(files)
        } else if formats.contains(&png_fmt) {
            let h = GetClipboardData(png_fmt);
            let p = GlobalLock(h) as *const u8;
            let data = std::slice::from_raw_parts(p, GlobalSize(h)).to_vec();
            GlobalUnlock(h);
            Clip::Png(data)
        } else {
            Clip::Nothing
        }
    };
    unsafe { CloseClipboard() };
    Ok((clip, names))
}

fn set_clipboard_text(text: &str) -> bool {
    const GMEM_MOVEABLE: u32 = 0x2;
    let w = wide(text);
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        let p = GlobalLock(h) as *mut u16;
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        GlobalUnlock(h);
        if !open_clipboard() { return false; }
        EmptyClipboard();
        let ok = !SetClipboardData(CF_UNICODETEXT, h).is_null();
        CloseClipboard();
        ok
    }
}

fn foreground_title() -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(GetForegroundWindow(), buf.as_mut_ptr(), 256) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

// The file name of the exe of the foreground window, or `unknown` when Windows gives none:
// no window, or a process that ferry may not query.
fn foreground_exe() -> String {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    let p = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if p.is_null() {
        return UNKNOWN_EXE.into();
    }
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(p, 0, buf.as_mut_ptr(), &mut n) };
    unsafe { CloseHandle(p) };
    match String::from_utf16_lossy(&buf[..n as usize]).rsplit('\\').next() {
        Some(name) if ok != 0 && !name.is_empty() => name.into(),
        _ => UNKNOWN_EXE.into(),
    }
}

// The clipboard as the items of a press. `None` when there is nothing to send: this plays the
// error sound and logs the cause.
fn read_items() -> Option<Vec<Item>> {
    let (clip, formats) = match read_clipboard() {
        Ok(c) => c,
        Err(e) => {
            log(&format!("  FAIL: {e}"));
            play(Sound::Error);
            return None;
        }
    };
    log(&format!("  clipboard formats: {}", formats.join(", ")));
    let items: Vec<Item> = match clip {
        Clip::Nothing => {
            log("  nothing to send (no HDROP, no PNG)");
            play(Sound::Error);
            return None;
        }
        Clip::Png(data) => vec![Item::mem("screenshot.png", data)],
        Clip::Files(files) => {
            if let Some(dir) = files.iter().find(|p| p.is_dir()) {
                log(&format!("  REFUSED: folder {}", dir.display()));
                play(Sound::Error);
                return None;
            }
            let mut v = Vec::new();
            for p in &files {
                match Item::file(p) {
                    Ok(i) => v.push(i),
                    Err(e) => {
                        log(&format!("  FAIL: read {}: {e}", p.display()));
                        play(Sound::Error);
                        return None;
                    }
                }
            }
            v
        }
    };
    Some(items)
}

// The copy of the clipboard is read here, at the press. The press itself runs on its own thread:
// it may wait for the channel, and the copy goes with it.
fn on_hotkey(link: &Arc<Link>) {
    let t = Instant::now();
    log(&format!("HOTKEY, foreground exe {}, window {:?}", foreground_exe(), foreground_title()));
    let ex = exchange::folder();
    let Some(items) = read_items() else {
        // Nothing to send: a press while not connected still starts the connect.
        return exchange::start_connect(&ex, || link.press());
    };
    let bytes: u64 = items.iter().map(|i| i.size).sum();
    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    log(&format!("  {} item(s), {bytes} B: {}", items.len(), names.join(", ")));
    let link = link.clone();
    std::thread::spawn(move || {
        exchange::press(&ex, &items, exchange::HELLO_WAIT, || link.press(), set_clipboard_text);
        log(&format!("  hotkey to reply {} ms", t.elapsed().as_millis()));
    });
}

// What the foreground callback needs: it gets no argument of ours.
struct Ctx {
    link: Arc<Link>,
    key: String,
    mods: u32,
    vk: u32,
    active: Mutex<Active>,
}

static CTX: OnceLock<Ctx> = OnceLock::new();

// Registers the key when the foreground exe is on the list and releases it otherwise, then
// tells the link. It runs on the thread of the message loop, as `RegisterHotKey` needs.
fn check() {
    let Some(c) = CTX.get() else { return };
    let exe = foreground_exe();
    let mut a = c.active.lock().unwrap_or_else(|e| e.into_inner());
    match a.focus(&exe) {
        Act::Register => {
            let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), 1, c.mods | MOD_NOREPEAT, c.vk) } != 0;
            let err = last_error();
            let t = a.registered(ok);
            if t.log {
                log(&format!("FAIL: RegisterHotKey {}, error {err} (1409 = another program has it)", c.key));
            }
            if t.sound {
                play(Sound::Error);
            }
        }
        Act::Unregister => {
            unsafe { UnregisterHotKey(std::ptr::null_mut(), 1) };
            a.unregistered();
        }
        Act::Keep => {}
    }
    let on = a.listed(&exe);
    c.link.focus(Seen { taken: a.taken().then(|| c.key.clone()), exe: Some((exe, on)) });
}

unsafe extern "system" fn on_focus(_hook: H, _event: u32, _hwnd: H, _object: i32, _child: i32, _thread: u32, _time: u32) {
    check();
}

// Runs on the thread that calls it: `RegisterHotKey` binds to that thread, and the
// out-of-context event hook calls back from the message loop of that thread.
pub fn run(o: &Opts, link: &Arc<Link>) {
    let _ = CTX.set(Ctx { link: link.clone(), key: o.hotkey.clone(), mods: o.mods, vk: o.vk, active: Mutex::new(Active::new(&o.apps)) });
    if unsafe { SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, std::ptr::null_mut(), on_focus, 0, 0, WINEVENT_OUTOFCONTEXT) }.is_null() {
        return log(&format!("FAIL: SetWinEventHook, error {}", last_error()));
    }
    log(&format!("ready: {} while {} has focus", o.hotkey, o.apps.join(", ")));
    check();

    let mut msg: Msg = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        if msg.message == WM_HOTKEY {
            on_hotkey(link);
        } else {
            unsafe { DispatchMessageW(&msg) };
        }
    }
}

// The message loop for a process whose hotkey is not registered: the hidden
// window of the tray icon still needs its messages.
pub fn pump() {
    let mut msg: Msg = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe { DispatchMessageW(&msg) };
    }
}
