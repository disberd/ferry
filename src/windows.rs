// Workstation commands and the Windows helpers shared by `exchange` and
// `hotkey`.
//   start <alias>  start a detached `run <alias>`; its output goes to the log
//   run <alias>    the Exchange folder loop (thread) and the hotkey (main thread)
//   stop           kill the running `run` and its ssh.exe children
//   status         show version, pid and log path
// State in %LOCALAPPDATA%\ferry\: ferry.log, state (the pid of `run`).

use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attrs: *mut c_void, owner: i32, name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
    fn QueryFullProcessImageNameW(h: *mut c_void, flags: u32, name: *mut u16, len: *mut u32) -> i32;
    fn SetFileAttributesW(path: *const u16, attrs: u32) -> i32;
}

const DETACHED_PROCESS: u32 = 0x8;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_ALREADY_EXISTS: u32 = 183;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const STILL_ACTIVE: u32 = 259;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

pub fn hide(p: &std::path::Path) {
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    unsafe { SetFileAttributesW(wide(&p.display().to_string()).as_ptr(), FILE_ATTRIBUTE_HIDDEN) };
}

// UTC `YYYY-MM-DD HH:MM:SS` from the system clock (days to date: Hinnant's civil_from_days).
fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

// Never panics: stdout can be closed.
pub fn log(msg: &str) {
    let _ = writeln!(io::stdout().lock(), "{} {msg}", timestamp());
}

// Hidden ssh.exe with the Remote command; piped stdio.
pub fn ssh(alias: &str, remote: &str) -> Command {
    let system_ssh = r"C:\Windows\System32\OpenSSH\ssh.exe";
    let ssh = if std::path::Path::new(system_ssh).exists() { system_ssh } else { "ssh.exe" };
    let mut c = Command::new(ssh);
    c.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=15", alias, remote]);
    c.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c
}

fn dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA is not set")).join("ferry")
}

fn saved_pid() -> Option<u32> {
    fs::read_to_string(dir().join("state")).ok()?.trim().parse().ok()
}

// True if `pid` runs and its exe is `ferry.exe`, so a reused pid is not killed.
fn alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0
            && code == STILL_ACTIVE
            && QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0
            && String::from_utf16_lossy(&buf[..len as usize]).to_lowercase().ends_with("\\ferry.exe");
        CloseHandle(h);
        ok
    }
}

pub fn start(alias: &str) -> i32 {
    if let Some(pid) = saved_pid().filter(|&p| alive(p)) {
        println!("already running (pid {pid})");
        return 0;
    }
    let d = dir();
    let go = || -> io::Result<u32> {
        fs::create_dir_all(&d)?;
        let log = OpenOptions::new().create(true).append(true).open(d.join("ferry.log"))?;
        let child = Command::new(std::env::current_exe()?)
            .args(["run", alias])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
        fs::write(d.join("state"), child.id().to_string())?;
        Ok(child.id())
    };
    match go() {
        Ok(pid) => {
            println!("started (pid {pid})");
            0
        }
        Err(e) => {
            eprintln!("ferry: start failed: {e}");
            1
        }
    }
}

pub fn run(alias: &str) -> i32 {
    let m = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide(r"Local\ferry").as_ptr()) };
    if m.is_null() || last_error() == ERROR_ALREADY_EXISTS {
        println!("ferry already runs");
        return 0;
    }
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    log(&format!("ferry {} run {alias} exe {exe}", crate::frame::VERSION));
    let a = alias.to_string();
    std::thread::spawn(move || crate::exchange::run(&a));
    crate::hotkey::run(alias);
    // No hotkey: the Exchange folder loop keeps the process alive.
    loop {
        std::thread::park();
    }
}

pub fn stop() -> i32 {
    let Some(pid) = saved_pid().filter(|&p| alive(p)) else {
        println!("not running");
        return 0;
    };
    let killed = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    println!("{}", if killed { format!("stopped (pid {pid})") } else { "not running".into() });
    0
}

pub fn status() -> i32 {
    println!("ferry {}", crate::frame::VERSION);
    let pid = saved_pid().filter(|&p| alive(p));
    match pid {
        Some(p) => println!("run: pid {p}"),
        None => println!("run: not running"),
    }
    println!("log: {}", dir().join("ferry.log").display());
    i32::from(pid.is_none())
}
