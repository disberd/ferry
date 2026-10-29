// Workstation side of the Exchange folder path. Holds one hidden
// `ssh.exe -T <alias> ~/.local/bin/ferry serve` and reconnects when it ends.
// It answers `ls`, `pull` and `push` from agents in the Exchange folder
// %USERPROFILE%\ferry\.
//
// Only `run` and `connect` need Win32. `channel` and everything below it builds
// on Linux, where the tests run it against `remote::serve_on`.

use crate::frame::*;
use crate::sounds::{play, Sound};
#[cfg(windows)]
use crate::link::Link;
#[cfg(windows)]
use crate::windows::{hide, log, ssh};
#[cfg(not(windows))]
use crate::stub::log;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PREV: &str = ".ferry-prev";
const PART: &str = ".ferry-part";
// A push leaves at least this much free on the Workstation disk.
const FLOOR: u64 = 2 << 30;

struct Push {
    tmp: PathBuf,
    target: PathBuf,
    rel: String,
    file: Option<File>,
    left: u64,
    t: Instant,
}

// One connection. A new connection makes a new C.
struct C {
    out: Out,
    ex: PathBuf,
    rx_ms: AtomicU64,
    pushes: Mutex<HashMap<u32, Push>>,
    // The streams that this side answers (`ls`, `pull`), each with a flag for a CANCEL from the opener.
    answering: Mutex<HashMap<u32, bool>>,
}

impl C {
    // True when the opener cancelled the stream: the answer sends no more frames.
    fn cancelled(&self, id: u32) -> bool {
        self.answering.lock().unwrap().get(&id) == Some(&true)
    }
}

// The start of `ferry run`: makes the Exchange folder with `.ferry-prev\` and `.ferry-part\`,
// and deletes what an earlier run left in `.ferry-part\`.
pub fn prepare(ex: &Path) -> io::Result<()> {
    fs::create_dir_all(ex.join(PREV))?;
    let part = ex.join(PART);
    if part.exists() {
        fs::remove_dir_all(&part)?;
    }
    fs::create_dir_all(part)
}

#[cfg(windows)]
pub fn run(alias: &str, link: &Link) {
    let remote = "~/.local/bin/ferry serve";
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    let ex = PathBuf::from(std::env::var("USERPROFILE").expect("USERPROFILE is not set")).join("ferry");
    prepare(&ex).unwrap();
    hide(&ex.join(PREV));
    hide(&ex.join(PART));
    log(&format!("Exchange folder {}, host {host}", ex.display()));

    loop {
        link.connecting();
        log(&format!("connect: {alias} {remote}"));
        let last = Arc::new(Mutex::new(String::new()));
        let why = match connect(alias, remote, &host, &ex, link, &last) {
            Ok(why) => why,
            Err(e) => format!("FAIL: {e}"),
        };
        log(&format!("connection ended: {why}"));
        let last = last.lock().unwrap().clone();
        link.pause(link.ended(&why, &last));
    }
}

#[cfg(windows)]
fn connect(alias: &str, remote: &str, host: &str, ex: &Path, link: &Link, last: &Arc<Mutex<String>>) -> Result<String, String> {
    let mut child = ssh(alias, remote)
        .map_err(|e| format!("spawn: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    let (done, drained) = std::sync::mpsc::channel::<()>();
    let last = last.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            log(&format!("ssh: {line}"));
            if !line.trim().is_empty() {
                *last.lock().unwrap() = line;
            }
        }
        let _ = done.send(());
    });
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let child: Arc<Mutex<Child>> = Arc::new(Mutex::new(child));
    let ch = child.clone();
    let why = channel(stdin, stdout, ex, host, move || {
        let _ = ch.lock().unwrap().kill();
    }, |f| link.hello(f));
    let _ = child.lock().unwrap().kill();
    let _ = child.lock().unwrap().wait();
    // Another process can hold the stderr pipe open, so the wait has a limit.
    let _ = drained.recv_timeout(Duration::from_secs(1));
    why
}

// One connection over any byte channel: the pipes of ssh.exe, or a socket pair
// in the tests. `stop` ends the channel when the Remote goes silent. `hello`
// gets the fields of the `HELLO` of the Remote, also when the protocol differs.
pub fn channel(mut stdin: impl Write + Send + 'static, r: impl Read, ex: &Path, host: &str, stop: impl Fn() + Send + 'static, hello: impl FnOnce(&[String])) -> Result<String, String> {
    let mut r = BufReader::with_capacity(CHUNK + 9, r);
    stdin.write_all(MAGIC).map_err(|e| e.to_string())?;
    let c = Arc::new(C {
        out: Out::default(),
        ex: ex.to_path_buf(),
        rx_ms: AtomicU64::new(now_ms()),
        pushes: Mutex::default(),
        answering: Mutex::default(),
    });
    c.out.ctrl(0, HELLO, format!("{PROTO}\t{VERSION}\t{host}").as_bytes());
    let c2 = c.clone();
    std::thread::spawn(move || c2.out.run(stdin));
    let c2 = c.clone();
    std::thread::spawn(move || {
        let mut n = 0u64;
        while !c2.out.dead() {
            std::thread::sleep(Duration::from_secs(1));
            n += 1;
            if n.is_multiple_of(PING_S) {
                c2.out.ctrl(0, PING, b"");
            }
            let silent = now_ms().saturating_sub(c2.rx_ms.load(SeqCst));
            if silent > DEAD_MS {
                log(&format!("Remote silent for {} s: stop ssh", silent / 1000));
                stop();
                return;
            }
        }
    });

    let why = match crate::link::handshake(&mut r) {
        Ok(f) => {
            hello(&f);
            if f[0] == PROTO {
                log(&format!("HELLO {}: ready", f.join(" ")));
                session(&c, &mut r)
            } else {
                format!("version mismatch: Workstation {PROTO} ({VERSION}), Remote {} ({})", f[0], f.get(1).map_or("?", String::as_str))
            }
        }
        Err(why) => why,
    };
    c.out.kill();
    for (_, p) in c.pushes.lock().unwrap().drain() {
        discard(p);
    }
    Ok(why)
}

fn session(c: &Arc<C>, r: &mut impl BufRead) -> String {
    loop {
        let (id, kind, p) = match read_frame(r) {
            Ok(f) => f,
            Err(e) => return format!("channel: {e}"),
        };
        c.rx_ms.store(now_ms(), SeqCst);
        match kind {
            PING => {}
            OPEN => open(c, id, fields(&p)),
            DATA => data(c, id, &p),
            CANCEL => {
                if let Some(p) = c.pushes.lock().unwrap().remove(&id) {
                    discard(p);
                    log(&format!("{id}: CANCEL, part file deleted"));
                } else if let Some(x) = c.answering.lock().unwrap().get_mut(&id) {
                    *x = true;
                } // else: the stream is closed or unknown, and the frame is dropped
            }
            // The Workstation opens no request that an END could close.
            END => {}
            BYE => return String::from_utf8_lossy(&p).to_string(),
            k => log(&format!("{id}: unknown frame kind {k}")),
        }
    }
}

fn reply(c: &C, id: u32, code: u8, msg: &str) {
    if c.cancelled(id) {
        return log(&format!("{id}: cancelled, no END"));
    }
    log(&format!("{id}: END {code} {msg}"));
    c.out.stream(id, END, &end(code, msg));
}

// Agent path to Exchange path.
fn resolve(ex: &Path, x: &str) -> Result<PathBuf, String> {
    if x.starts_with(['/', '\\']) || x.contains(':') {
        return Err(format!("not a relative path: {x}"));
    }
    let mut p = ex.to_path_buf();
    for (i, part) in x.split(['/', '\\']).filter(|s| !s.is_empty()).enumerate() {
        let stem = part.split('.').next().unwrap().to_ascii_uppercase();
        let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
            || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
        if part == ".."
            || part == "."
            || part.chars().any(|c| c.is_control() || "<>\"|?*".contains(c))
            || part.ends_with(['.', ' '])
            || reserved
            || (i == 0 && part.eq_ignore_ascii_case(PREV))
            || ferry_name(part)
        {
            return Err(format!("bad path: {x}"));
        }
        p.push(part);
    }
    Ok(p)
}

// A name that starts with `.ferry-`, in any case: the folders and part files of ferry.
fn ferry_name(s: &str) -> bool {
    s.to_ascii_lowercase().starts_with(".ferry-")
}

// Explorer reads these when it shows a folder, so an agent must not write them.
fn refused(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "desktop.ini" || [".url", ".lnk", ".library-ms", ".searchconnector-ms", ".scf"].iter().any(|e| n.ends_with(e))
}

// Free bytes on the volume of the Exchange folder. The fail-open value is for a call that fails.
#[cfg(windows)]
fn free_bytes(ex: &Path) -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(crate::windows::wide(&ex.display().to_string()).as_ptr(), &mut avail, &mut total, &mut free) };
    if ok == 0 {
        log(&format!("free space of {}: error {}", ex.display(), crate::windows::last_error()));
        return u64::MAX;
    }
    avail
}

// No volume query off Windows: the tests set the free space for one Exchange folder.
#[cfg(all(not(windows), test))]
pub static FREE: Mutex<std::collections::BTreeMap<PathBuf, u64>> = Mutex::new(std::collections::BTreeMap::new());

#[cfg(not(windows))]
fn free_bytes(ex: &Path) -> u64 {
    #[cfg(test)]
    {
        if let Some(n) = FREE.lock().unwrap().get(ex) {
            return *n;
        }
    }
    let _ = ex;
    u64::MAX
}

fn in_use(e: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, or access denied.
    matches!(e.raw_os_error(), Some(32 | 33)) || e.kind() == io::ErrorKind::PermissionDenied
}

fn open(c: &Arc<C>, id: u32, f: Vec<String>) {
    log(&format!("{id}: OPEN {}", f.join(" ")));
    let rel = f.get(1).cloned().unwrap_or_default();
    let path = match resolve(&c.ex, &rel) {
        Ok(p) => p,
        Err(e) => return reply(c, id, BAD_PATH, &e),
    };
    let c = c.clone();
    match f[0].as_str() {
        "ls" => answer(&c, id, move |c| ls(c, id, &path, &rel)),
        "pull" => answer(&c, id, move |c| pull(c, id, &path, &rel)),
        "push" => {
            let Some(size) = f.get(2).and_then(|s| s.parse::<u64>().ok()) else {
                return reply(&c, id, 1, "bad size");
            };
            if path == c.ex {
                return reply(&c, id, BAD_PATH, "push needs a file name");
            }
            let dir = path.parent().unwrap();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if refused(&name) {
                return reply(&c, id, BAD_PATH, &format!("refused file type: {name}"));
            }
            if size > CAP {
                return reply(&c, id, TOO_LARGE, &format!("{size} B is over the cap of {CAP} B"));
            }
            let free = free_bytes(&c.ex);
            if free.saturating_sub(size) < FLOOR {
                return reply(&c, id, TOO_LARGE, &format!("Workstation disk low: {} GiB free", free >> 30));
            }
            if path.is_dir() {
                return reply(&c, id, BAD_PATH, &format!("is a folder: {rel}"));
            }
            if dir.ancestors().take_while(|a| *a != c.ex).any(|a| a.is_file()) {
                return reply(&c, id, BAD_PATH, &format!("a parent of {rel} is a file"));
            }
            if let Err(e) = fs::create_dir_all(dir).and_then(|_| fs::create_dir_all(c.ex.join(PART))) {
                return reply(&c, id, 1, &e.to_string());
            }
            let tmp = c.ex.join(PART).join(format!("{id}-{name}"));
            let file = match File::create(&tmp) {
                Ok(f) => f,
                Err(e) => return reply(&c, id, 1, &e.to_string()),
            };
            let push = Push {
                tmp,
                rel,
                target: path,
                file: Some(file),
                left: size,
                t: Instant::now(),
            };
            if size == 0 {
                finish(&c, id, push);
            } else {
                c.pushes.lock().unwrap().insert(id, push);
            }
        }
        op => reply(&c, id, 1, &format!("unknown request {op}")),
    }
}

// Runs the answer to `id` on its own thread. A CANCEL for `id` stops its END.
fn answer(c: &Arc<C>, id: u32, f: impl FnOnce(&C) + Send + 'static) {
    c.answering.lock().unwrap().insert(id, false);
    let c = c.clone();
    std::thread::spawn(move || {
        f(&c);
        c.answering.lock().unwrap().remove(&id);
    });
}

fn ls(c: &C, id: u32, dir: &Path, rel_dir: &str) {
    if dir.is_file() {
        return reply(c, id, BAD_PATH, &format!("not a folder: {rel_dir}"));
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return reply(c, id, BAD_PATH, &format!("{rel_dir}: {e}")),
    };
    let rel = dir.strip_prefix(&c.ex).unwrap().to_string_lossy().replace('\\', "/");
    let mut lines: Vec<String> = rd
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            !ferry_name(&n) && !n.starts_with("~$")
        })
        .map(|e| {
            let m = e.metadata().unwrap();
            let t = m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            let name = e.file_name().to_string_lossy().to_string();
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            format!("{}\t{t}\t{path}{}\n", m.len(), if m.is_dir() { "/" } else { "" })
        })
        .collect();
    lines.sort_by(|a, b| a.split('\t').nth(2).cmp(&b.split('\t').nth(2)));
    for chunk in lines.concat().as_bytes().chunks(CHUNK) {
        c.out.stream(id, DATA, chunk);
    }
    reply(c, id, OK, "");
}

fn pull(c: &C, id: u32, path: &Path, rel: &str) {
    if path.is_dir() {
        return reply(c, id, BAD_PATH, &format!("is a folder: {rel}"));
    }
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if in_use(&e) => return reply(c, id, IN_USE, &format!("in use: {rel}")),
        Err(e) => return reply(c, id, BAD_PATH, &format!("{rel}: {e}")),
    };
    let size = f.metadata().unwrap().len();
    if size > CAP {
        return reply(c, id, TOO_LARGE, &format!("{size} B is over the cap of {CAP} B"));
    }
    let t = Instant::now();
    let mut buf = vec![0; CHUNK];
    loop {
        if c.cancelled(id) {
            return log(&format!("{id}: pull cancelled"));
        }
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if !c.out.stream(id, DATA, &buf[..n]) {
                    return;
                }
            }
            Err(e) => return reply(c, id, 1, &e.to_string()),
        }
    }
    reply(c, id, OK, &format!("{size} B in {} ms", t.elapsed().as_millis()));
}

fn data(c: &C, id: u32, d: &[u8]) {
    let mut m = c.pushes.lock().unwrap();
    let Some(p) = m.get_mut(&id) else { return };
    let err = if d.len() as u64 > p.left {
        Some("more data than declared".to_string())
    } else {
        p.file.as_mut().unwrap().write_all(d).err().map(|e| e.to_string())
    };
    if let Some(e) = err {
        let p = m.remove(&id).unwrap();
        drop(m);
        discard(p);
        return reply(c, id, 1, &e);
    }
    p.left -= d.len() as u64;
    if p.left == 0 {
        let p = m.remove(&id).unwrap();
        drop(m);
        finish(c, id, p);
    }
}

// Closes the part file of a push and deletes it.
fn discard(p: Push) {
    drop(p.file);
    let _ = fs::remove_file(&p.tmp);
}

fn finish(c: &C, id: u32, mut p: Push) {
    p.file = None; // Windows renames only a closed file
    let secs = p.t.elapsed().as_secs_f64();
    #[cfg(windows)]
    {
        if let Err(e) = crate::windows::mark_of_the_web(&p.tmp) {
            log(&format!("Zone.Identifier of {}: {e}", p.rel));
        }
    }
    let r = place(c, &p);
    let (code, msg) = match r {
        Ok(m) => (OK, format!("{m} ({secs:.1} s)")),
        Err((code, m)) => {
            let _ = fs::remove_file(&p.tmp);
            (code, m)
        }
    };
    reply(c, id, code, &msg);
    if code == OK {
        play(Sound::Arrival);
    }
}

// Rename the old file to .ferry-prev, then the part file to the target. A
// locked file (Excel) refuses the first rename, so nothing changes.
fn place(c: &C, p: &Push) -> Result<String, (u8, String)> {
    if p.target.exists() {
        let prev = c.ex.join(PREV).join(p.target.strip_prefix(&c.ex).unwrap());
        fs::create_dir_all(prev.parent().unwrap()).map_err(|e| (1, e.to_string()))?;
        match fs::rename(&p.target, &prev) {
            Ok(()) => {}
            Err(e) if in_use(&e) => {
                log(&format!("{}: {e}", p.rel));
                return Err((IN_USE, format!("in use: {}", p.rel)));
            }
            Err(e) => return Err((1, e.to_string())),
        }
    }
    fs::rename(&p.tmp, &p.target).map_err(|e| (1, e.to_string()))?;
    Ok(format!("saved: {}", p.rel))
}
