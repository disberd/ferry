// Workstation side of the hotkey flow. Opens one hidden
// `ssh.exe -T <alias> ~/.local/bin/ferry hk-serve`, then waits for Ctrl+Alt+V.
// On the hotkey it reads the clipboard: files (CF_HDROP) first, else the PNG
// image, else an error sound. It sends them to `hk-serve`, which types the
// Remote paths into the focused herdr pane. If `hk-serve` cannot paste, the
// paths go to the clipboard. Every step is logged.

use std::ffi::c_void;
use crate::sounds::{play, Sound};
use crate::windows::{last_error, log, ssh, wide};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout};
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
const MOD_ALT: u32 = 0x1;
const MOD_CONTROL: u32 = 0x2;
const MOD_NOREPEAT: u32 = 0x4000;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const REMOTE: &str = "~/.local/bin/ferry hk-serve";

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

struct Conn {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

fn connect(alias: &str) -> std::io::Result<Conn> {
    log(&format!("connect: {alias} {REMOTE}"));
    let mut child = ssh(alias, REMOTE).spawn()?;
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            log(&format!("  remote stderr: {line}"));
        }
    });
    Ok(Conn { stdin: child.stdin.take().unwrap(), stdout: BufReader::new(child.stdout.take().unwrap()), _child: child })
}

fn send(conn: &mut Conn, items: &[(String, Vec<u8>)]) -> std::io::Result<String> {
    write!(conn.stdin, "SEND {}\n", items.len())?;
    for (name, data) in items {
        write!(conn.stdin, "{} {name}\n", data.len())?;
        conn.stdin.write_all(data)?;
    }
    conn.stdin.flush()?;
    let mut reply = String::new();
    if conn.stdout.read_line(&mut reply)? == 0 {
        return Err(std::io::Error::other("serve closed the channel"));
    }
    Ok(reply.trim_end().to_string())
}

fn on_hotkey(conn: &mut Option<Conn>, alias: &str) {
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
    let items: Vec<(String, Vec<u8>)> = match clip {
        Clip::Nothing => {
            log("  nothing to send (no HDROP, no PNG)");
            return play(Sound::Error);
        }
        Clip::Png(data) => vec![("screenshot.png".into(), data)],
        Clip::Files(files) => {
            if let Some(dir) = files.iter().find(|p| p.is_dir()) {
                log(&format!("  REFUSED: folder {}", dir.display()));
                return play(Sound::Error);
            }
            let mut v = Vec::new();
            for p in &files {
                match std::fs::read(p) {
                    Ok(d) => v.push((p.file_name().unwrap().to_string_lossy().into_owned(), d)),
                    Err(e) => {
                        log(&format!("  FAIL: read {}: {e}", p.display()));
                        return play(Sound::Error);
                    }
                }
            }
            v
        }
    };
    let bytes: usize = items.iter().map(|(_, d)| d.len()).sum();
    let names: Vec<&str> = items.iter().map(|(n, _)| n.as_str()).collect();
    log(&format!("  read {} item(s), {bytes} B in {} ms: {}", items.len(), t.elapsed().as_millis(), names.join(", ")));

    let mut reply = conn.as_mut().map_or(Err(std::io::Error::other("no channel")), |c| send(c, &items));
    if let Err(e) = &reply {
        log(&format!("  channel error: {e}, reconnect and retry once"));
        *conn = connect(alias).ok();
        reply = conn.as_mut().map_or(Err(std::io::Error::other("no channel")), |c| send(c, &items));
    }
    let reply = match reply {
        Ok(r) => r,
        Err(e) => {
            *conn = None;
            log(&format!("  FAIL: {e}"));
            return play(Sound::Error);
        }
    };
    let total = t.elapsed().as_millis();
    let parts: Vec<&str> = reply.split('\t').collect();
    match parts.as_slice() {
        ["PASTED", pane, agent, ms, paths] => {
            log(&format!("  PASTED into {pane} (agent {agent}): {paths}"));
            log(&format!("  hotkey to reply {total} ms, of which serve save+paste {ms} ms"));
        }
        ["CLIP", reason, ms, paths] => {
            let ok = set_clipboard_text(paths);
            log(&format!("  NO PASTE: {reason}"));
            log(&format!("  paths on Workstation clipboard ({}): {paths}", if ok { "ok" } else { "FAILED" }));
            log(&format!("  hotkey to reply {total} ms, of which serve {ms} ms"));
            play(Sound::Clip);
        }
        _ => {
            log(&format!("  FAIL: bad reply {reply:?}"));
            play(Sound::Error);
        }
    }
}

// Runs on the thread that calls it: RegisterHotKey binds to that thread.
pub fn run(alias: &str) {
    if unsafe { RegisterHotKey(std::ptr::null_mut(), 1, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'V' as u32) } == 0 {
        return log(&format!("FAIL: RegisterHotKey Ctrl+Alt+V, error {} (1409 = another program has it)", last_error()));
    }
    let mut conn = match connect(alias) {
        Ok(c) => Some(c),
        Err(e) => {
            log(&format!("FAIL: spawn ssh.exe: {e}"));
            None
        }
    };
    log("ready: press Ctrl+Alt+V");

    let mut msg: Msg = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        if msg.message == WM_HOTKEY {
            on_hotkey(&mut conn, alias);
        }
    }
}
