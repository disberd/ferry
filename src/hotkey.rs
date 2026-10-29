// Workstation side of the hotkey flow. It waits for Ctrl+Alt+V. On the hotkey it reads the
// clipboard: files (CF_HDROP) first, else the PNG image, else an error sound. `exchange::press`
// sends them to `ferry serve` over the channel; `serve` types the Remote paths into the focused
// herdr pane, or the paths go to the clipboard. The press runs on its own thread, so the
// message loop (and the tray icon) stays live during a transfer. Every step is logged.

use std::ffi::c_void;
use crate::exchange::{self, Item};
use crate::lifecycle::Opts;
use crate::sounds::{play, Sound};
use crate::windows::{last_error, log, wide};
use std::path::PathBuf;
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
}
#[link(name = "shell32")]
extern "system" {
    fn DragQueryFileW(h: H, i: u32, buf: *mut u16, n: u32) -> u32;
}
const WM_HOTKEY: u32 = 0x0312;
const MOD_NOREPEAT: u32 = 0x4000;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

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

fn on_hotkey() {
    let t = Instant::now();
    log(&format!("HOTKEY, foreground window: {:?}", foreground_title()));
    let (clip, formats) = match read_clipboard() {
        Ok(c) => c,
        Err(e) => {
            log(&format!("  FAIL: {e}"));
            return play(Sound::Error);
        }
    };
    log(&format!("  clipboard formats: {}", formats.join(", ")));
    let items: Vec<Item> = match clip {
        Clip::Nothing => {
            log("  nothing to send (no HDROP, no PNG)");
            return play(Sound::Error);
        }
        Clip::Png(data) => vec![Item::mem("screenshot.png", data)],
        Clip::Files(files) => {
            if let Some(dir) = files.iter().find(|p| p.is_dir()) {
                log(&format!("  REFUSED: folder {}", dir.display()));
                return play(Sound::Error);
            }
            let mut v = Vec::new();
            for p in &files {
                match Item::file(p) {
                    Ok(i) => v.push(i),
                    Err(e) => {
                        log(&format!("  FAIL: read {}: {e}", p.display()));
                        return play(Sound::Error);
                    }
                }
            }
            v
        }
    };
    let bytes: u64 = items.iter().map(|i| i.size).sum();
    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    log(&format!("  {} item(s), {bytes} B: {}", items.len(), names.join(", ")));
    std::thread::spawn(move || {
        exchange::press(&exchange::folder(), &items, set_clipboard_text);
        log(&format!("  hotkey to reply {} ms", t.elapsed().as_millis()));
    });
}

// Runs on the thread that calls it: RegisterHotKey binds to that thread.
pub fn run(o: &Opts) {
    if unsafe { RegisterHotKey(std::ptr::null_mut(), 1, o.mods | MOD_NOREPEAT, o.vk) } == 0 {
        return log(&format!("FAIL: RegisterHotKey {}, error {} (1409 = another program has it)", o.hotkey, last_error()));
    }
    log(&format!("ready: press {}", o.hotkey));

    let mut msg: Msg = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        if msg.message == WM_HOTKEY {
            on_hotkey();
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
