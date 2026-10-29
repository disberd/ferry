// Workstation commands and the Windows helpers shared by `exchange` and
// `hotkey`.
//   start <alias> [--hotkey K] [--apps a,b]
//                  start a detached `run` with the same arguments
//   run <alias> ...  the Exchange folder loop (thread), the hotkey and the tray icon (main thread)
//   stop           end the running `run`; its ssh.exe children end with it
//   status         show the state of `run` in five lines; exit 0 when connected
// State in %LOCALAPPDATA%\ferry\: ferry.log (and ferry.log.1), state (pid,
// process start time and build version of `run`, then its status text).

use crate::lifecycle::{self, Opts, State};
use crate::link::{self, Link, Report};
use std::ffi::c_void;
use std::fs;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attrs: *mut c_void, owner: i32, name: *const u16) -> *mut c_void;
    fn OpenMutexW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetProcessTimes(h: *mut c_void, created: *mut u64, exited: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    fn TerminateProcess(h: *mut c_void, code: u32) -> i32;
    fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
    fn CloseHandle(h: *mut c_void) -> i32;
    fn SetFileAttributesW(path: *const u16, attrs: u32) -> i32;
    fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(job: *mut c_void, class: i32, info: *mut c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
    fn GetLocalTime(t: *mut [u16; 8]);
}

const DETACHED_PROCESS: u32 = 0x8;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_ALREADY_EXISTS: u32 = 183;
const PROCESS_TERMINATE: u32 = 0x1;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const SYNCHRONIZE: u32 = 0x10_0000;
const WAIT_OBJECT_0: u32 = 0;
const MUTEX: &str = r"Local\ferry";

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

// Marks a file as from the Internet zone. The stream stays when the file is renamed.
pub fn mark_of_the_web(p: &std::path::Path) -> io::Result<()> {
    let mut s = p.as_os_str().to_owned();
    s.push(":Zone.Identifier");
    fs::write(s, "[ZoneTransfer]\r\nZoneId=3\r\n")
}

fn dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA is not set")).join("ferry")
}

// Writes to `ferry.log`. Never fails or panics: no folder, a full disk or a
// poisoned lock only lose the line.
pub fn log(msg: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _one_writer = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = std::env::var_os("LOCALAPPDATA") {
        lifecycle::log_line(&PathBuf::from(d).join("ferry").join("ferry.log"), msg);
    }
}

// The local time as `HH:MM`.
fn local_hhmm() -> String {
    let mut t = [0u16; 8]; // SYSTEMTIME: year, month, weekday, day, hour, minute, second, ms
    unsafe { GetLocalTime(&mut t) };
    format!("{:02}:{:02}", t[4], t[5])
}

// Replaces the state file with the first line of `s` and the report. A reader
// never sees half a file.
fn write_state(s: &State, r: &Report) {
    let (path, tmp) = (dir().join("state"), dir().join("state.tmp"));
    if let Err(e) = fs::write(&tmp, format!("{}{}", s.text(), r.text())).and_then(|_| fs::rename(&tmp, &path)) {
        log(&format!("FAIL: write state file: {e}"));
    }
}

// Hidden ssh.exe with the Remote command; piped stdio. It runs in the Job
// object of `ferry run`, so it ends when `ferry run` ends, however that happens.
pub fn ssh(alias: &str, remote: &str) -> io::Result<Child> {
    let system_ssh = r"C:\Windows\System32\OpenSSH\ssh.exe";
    let ssh = if std::path::Path::new(system_ssh).exists() { system_ssh } else { "ssh.exe" };
    let mut c = Command::new(ssh);
    c.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=15", "-o", "ClearAllForwardings=yes", "-o", "RemoteCommand=none", alias, remote]);
    c.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn()?;
    in_job(&child);
    Ok(child)
}

// Layout of JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
#[repr(C)]
struct JobLimits {
    per_process_time: i64,
    per_job_time: i64,
    flags: u32,
    min_working_set: usize,
    max_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
    io_counters: [u64; 6],
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}

// The job handle (as usize, 0 = none) lives until the process ends. Windows
// closes it then, and `KILL_ON_JOB_CLOSE` ends every process in the job.
fn in_job(child: &Child) {
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    static JOB: LazyLock<usize> = LazyLock::new(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        let mut limits: JobLimits = std::mem::zeroed();
        limits.flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = std::mem::size_of::<JobLimits>() as u32;
        if job.is_null() || SetInformationJobObject(job, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, std::ptr::addr_of_mut!(limits).cast(), size) == 0 {
            log(&format!("FAIL: Job object for ssh.exe, error {}", last_error()));
            return 0;
        }
        job as usize
    });
    let job = *JOB;
    if job != 0 && unsafe { AssignProcessToJobObject(job as *mut c_void, child.as_raw_handle()) } == 0 {
        log(&format!("FAIL: ssh.exe not in the Job object, error {}", last_error()));
    }
}

fn state() -> Option<State> {
    State::parse(&fs::read_to_string(dir().join("state")).ok()?)
}

// The creation time of a process, as the 64-bit value of a FILETIME.
fn start_time(h: *mut c_void) -> Option<u64> {
    let mut t = [0u64; 4];
    (unsafe { GetProcessTimes(h, &mut t[0], &mut t[1], &mut t[2], &mut t[3]) } != 0).then_some(t[0])
}

// The `ferry run` of the state file, if it still runs. Windows uses pids again,
// so the saved start time must match too. The caller closes the handle.
fn saved_process() -> Option<(State, *mut c_void)> {
    let s = state()?;
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE, 0, s.pid) };
    if h.is_null() {
        return None;
    }
    if start_time(h) == Some(s.start) && unsafe { WaitForSingleObject(h, 0) } != WAIT_OBJECT_0 {
        return Some((s, h));
    }
    unsafe { CloseHandle(h) };
    None
}

// True while a `ferry run` holds the mutex.
fn mutex_held() -> bool {
    let m = unsafe { OpenMutexW(SYNCHRONIZE, 0, wide(MUTEX).as_ptr()) };
    if !m.is_null() {
        unsafe { CloseHandle(m) };
    }
    !m.is_null()
}

pub fn start(o: &Opts) -> i32 {
    if mutex_held() {
        // `ferry run` writes the state file just after it takes the mutex.
        println!("ferry already runs (pid {})", state().map_or("unknown".into(), |s| s.pid.to_string()));
        return 0;
    }
    let go = || -> io::Result<u32> {
        let child = Command::new(std::env::current_exe()?)
            .args(o.run_args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
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

pub fn run(o: &Opts) -> i32 {
    let m = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide(MUTEX).as_ptr()) };
    if m.is_null() || last_error() == ERROR_ALREADY_EXISTS {
        return 0;
    }
    let _ = fs::create_dir_all(dir());
    lifecycle::panic_hook(log);
    let s = State { pid: std::process::id(), start: start_time(unsafe { GetCurrentProcess() }).unwrap_or(0), version: crate::frame::VERSION.into() };
    if let Err(e) = fs::write(dir().join("state"), s.text()) {
        log(&format!("FAIL: write state file: {e}"));
    }
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    log(&format!("ferry {} run {} exe {exe}, hotkey {}, apps {}", crate::frame::VERSION, o.alias, o.hotkey, o.apps.join(",")));
    let link = std::sync::Arc::new(Link::new(&o.alias, &format!("{} for {}", o.hotkey, o.apps.join(", ")), local_hhmm, {
        let s = s.clone();
        move |r| {
            log(&format!("channel: {}", r.channel));
            write_state(&s, r);
            crate::tray::changed();
        }
    }));
    crate::tray::create(link.clone(), s, dir().join("ferry.log").display().to_string());
    let a = o.alias.clone();
    let l = link.clone();
    std::thread::spawn(move || crate::exchange::run(&a, &l));
    crate::hotkey::run(o, &link);
    // No hotkey: the window of the tray icon still needs its messages.
    crate::hotkey::pump();
    1
}

pub fn stop() -> i32 {
    let Some((s, h)) = saved_process() else {
        println!("not running");
        return 0;
    };
    // A killed process cannot clean up: the Job object ends its ssh.exe.
    let ended = unsafe { TerminateProcess(h, 1) != 0 && WaitForSingleObject(h, 5000) == WAIT_OBJECT_0 };
    unsafe { CloseHandle(h) };
    if ended {
        println!("stopped (pid {})", s.pid);
        0
    } else {
        eprintln!("ferry: pid {} did not end", s.pid);
        1
    }
}

pub fn status() -> i32 {
    let text = fs::read_to_string(dir().join("state")).unwrap_or_default();
    let running = saved_process().map(|(_, h)| unsafe { CloseHandle(h) }).is_some();
    let report = text.split_once('\n').and_then(|(_, rest)| Report::parse(rest));
    let (out, code) = link::status_text(crate::frame::VERSION, State::parse(&text).as_ref(), running, report.as_ref(), &dir().join("ferry.log").display().to_string());
    print!("{out}");
    code
}
