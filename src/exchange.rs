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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex};
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
    // The hotkey presses that this side opened (odd ids), each with the END of the Remote once it came.
    presses: Mutex<HashMap<u32, Option<(u8, String)>>>,
    press_ended: Condvar,
    press_next: AtomicU32,
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
    let ex = folder();
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
    }, |f| link.hello(f), |f| link.herdr(f));
    let _ = child.lock().unwrap().kill();
    let _ = child.lock().unwrap().wait();
    // Another process can hold the stderr pipe open, so the wait has a limit.
    let _ = drained.recv_timeout(Duration::from_secs(1));
    why
}

// One connection over any byte channel: the pipes of ssh.exe, or a socket pair
// in the tests. `stop` ends the channel when the Remote goes silent. `hello`
// gets the fields of the `HELLO` of the Remote, also when the protocol differs.
// `herdr` gets the fields of each `HERDR` frame and says whether the error sound is due.
pub fn channel(mut stdin: impl Write + Send + 'static, r: impl Read, ex: &Path, host: &str, stop: impl Fn() + Send + 'static, hello: impl FnOnce(&[String]), herdr: impl Fn(&[String]) -> bool) -> Result<String, String> {
    let mut r = BufReader::with_capacity(CHUNK + 9, r);
    // MAGIC and HELLO go out at once, not through the queue of `Out`: a mismatched
    // HELLO of the Remote ends the channel, and the Remote must still get ours.
    stdin.write_all(&[MAGIC.as_slice(), &frame(0, HELLO, format!("{PROTO}\t{VERSION}\t{host}").as_bytes())].concat()).map_err(|e| e.to_string())?;
    let c = Arc::new(C {
        out: Out::default(),
        ex: ex.to_path_buf(),
        rx_ms: AtomicU64::new(now_ms()),
        pushes: Mutex::default(),
        answering: Mutex::default(),
        presses: Mutex::default(),
        press_ended: Condvar::new(),
        press_next: AtomicU32::new(1),
    });
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
                LIVE.lock().unwrap().insert(ex.to_path_buf(), c.clone());
                LIVE_UP.notify_all();
                let why = session(&c, &mut r, &herdr);
                let mut live = LIVE.lock().unwrap();
                if live.get(ex).is_some_and(|l| Arc::ptr_eq(l, &c)) {
                    live.remove(ex);
                }
                why
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

fn session(c: &Arc<C>, r: &mut impl BufRead, herdr: &impl Fn(&[String]) -> bool) -> String {
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
            // The Remote answers a press with one END. The Workstation opens no other request.
            END => press_ended(c, id, &p),
            BYE => return String::from_utf8_lossy(&p).to_string(),
            ALERT => match fields(&p)[0].as_str() {
                "blocked" => play(Sound::Blocked),
                "done" => play(Sound::Done),
                s => log(&format!("ALERT with status {s}")),
            },
            HERDR => {
                if herdr(&fields(&p)) {
                    play(Sound::Error);
                }
            }
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

// ---- hotkey press ----

// The Exchange folder of the Workstation. It names the live channel for `press`.
#[cfg(windows)]
pub fn folder() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").expect("USERPROFILE is not set")).join("ferry")
}

// The channel of each Exchange folder, from its `HELLO` to its end. A press finds its channel here.
// Only a `HELLO` with the same protocol integer puts a channel in. `LIVE_UP` wakes the presses
// that wait for one.
static LIVE: Mutex<std::collections::BTreeMap<PathBuf, Arc<C>>> = Mutex::new(std::collections::BTreeMap::new());
static LIVE_UP: Condvar = Condvar::new();

// The newest press of each Exchange folder, by number. A press that waits for a channel gives
// up its copy when a newer press exists: only the newest copy of the clipboard is ever sent.
static LATEST: Mutex<std::collections::BTreeMap<PathBuf, u64>> = Mutex::new(std::collections::BTreeMap::new());
static PRESSES: AtomicU64 = AtomicU64::new(0);

// How long a press waits for a matching `HELLO` before it drops its copy.
#[cfg(windows)]
pub const HELLO_WAIT: Duration = Duration::from_secs(10);

pub fn connected(ex: &Path) -> bool {
    LIVE.lock().unwrap().contains_key(ex)
}

// A press, also one with nothing to send: calls `connect` when no channel is up.
pub fn start_connect(ex: &Path, connect: impl FnOnce()) {
    if !connected(ex) {
        connect();
    }
}

// A press holds at most this many files.
const MAX_FILES: usize = 100;

pub enum Src {
    File(PathBuf),
    Mem(Vec<u8>),
}

// One file of a press. A press sends exactly `size` bytes of it.
pub struct Item {
    pub name: String,
    pub size: u64,
    pub src: Src,
}

impl Item {
    // A file on disk, with its size now.
    pub fn file(p: &Path) -> io::Result<Item> {
        let name = p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Ok(Item { name, size: fs::metadata(p)?.len(), src: Src::File(p.to_path_buf()) })
    }

    pub fn mem(name: &str, data: Vec<u8>) -> Item {
        Item { name: name.into(), size: data.len() as u64, src: Src::Mem(data) }
    }
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    // The Remote typed the paths into a herdr pane.
    Pasted,
    // The paths are on the clipboard, or `set_clip` failed.
    Clip(Vec<String>),
    // The cause is in the text; the press gave the error sound.
    Failed(String),
    // A newer press took over while this one waited for the channel. This one sent nothing and
    // gave no sound.
    Superseded,
}

// The channel for a press: at once when it is up, else when a matching `HELLO` comes within
// `wait`. `Err` is the end of the press: the wait ran out, or a newer press took over.
fn await_channel(ex: &Path, wait: Duration) -> Result<Arc<C>, Outcome> {
    let end = Instant::now() + wait;
    let me = PRESSES.fetch_add(1, SeqCst);
    let mut live = LIVE.lock().unwrap();
    LATEST.lock().unwrap().insert(ex.to_path_buf(), me);
    LIVE_UP.notify_all(); // an older press that waits sees that it is not the newest
    loop {
        if LATEST.lock().unwrap().get(ex) != Some(&me) {
            return Err(Outcome::Superseded);
        }
        if let Some(c) = live.get(ex) {
            return Ok(c.clone());
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Outcome::Failed(format!("press: no HELLO within {wait:?}, the copy is dropped")));
        }
        live = LIVE_UP.wait_timeout(live, left).unwrap().0;
    }
}

// One hotkey press: sends `items` over the channel of the Exchange folder `ex`, waits for the
// reply and acts on it. With no channel up it calls `connect` and holds the copy in `items` until
// a matching `HELLO` comes, but for `wait` at most; then it drops the copy, so an old clipboard is
// never sent later. `set_clip` puts text on the clipboard of the Workstation; it gets the
// paths of a `clip` reply that passed the clip check. This plays the sound and logs the cause of
// a failure. It blocks until the reply, so a caller keeps it off its message loop.
pub fn press(ex: &Path, items: &[Item], wait: Duration, connect: impl FnOnce(), set_clip: impl FnOnce(&str) -> bool) -> Outcome {
    start_connect(ex, connect);
    let out = match await_channel(ex, wait) {
        Ok(c) => send(&c, items),
        Err(out) => out,
    };
    match &out {
        Outcome::Pasted | Outcome::Superseded => {}
        Outcome::Clip(paths) => {
            let text = paths.join(" ");
            let ok = set_clip(&text);
            log(&format!("press: paths on the clipboard ({}): {text}", if ok { "ok" } else { "FAILED" }));
            play(if ok { Sound::Clip } else { Sound::Error });
        }
        Outcome::Failed(why) => {
            log(why);
            play(Sound::Error);
        }
    }
    out
}

fn send(c: &Arc<C>, items: &[Item]) -> Outcome {
    let mut open = format!("hotkey\t{}", items.len());
    for it in items {
        open += &format!("\t{}\t{}", it.name.replace('\t', "_"), it.size);
    }
    if items.len() > MAX_FILES || open.len() > CHUNK {
        return Outcome::Failed(format!("press refused: {} files, OPEN of {} B", items.len(), open.len()));
    }
    let id = c.press_next.fetch_add(2, SeqCst);
    c.presses.lock().unwrap().insert(id, None);
    c.out.ctrl(id, OPEN, open.as_bytes());
    log(&format!("{id}: OPEN hotkey, {} files", items.len()));
    if let Err(why) = feed(c, id, items) {
        c.out.ctrl(id, CANCEL, b"");
        c.presses.lock().unwrap().remove(&id);
        return Outcome::Failed(format!("press {id}: {why}, CANCEL"));
    }
    let (code, text) = c.wait_end(id);
    log(&format!("{id}: END {code} {text}"));
    match (code, text.as_str()) {
        (OK, "pasted") => Outcome::Pasted,
        (OK, t) => match t.strip_prefix("clip ").and_then(clip_paths) {
            Some(paths) => Outcome::Clip(paths),
            None => Outcome::Failed(format!("press: the clip reply fails the check: {t}")),
        },
        _ => Outcome::Failed(format!("press: END {code} {text}")),
    }
}

// The paths of a `clip` reply, if each one matches `^/[A-Za-z0-9._/-]+$`. A path that
// passes holds no space, quote or control character, so a paste of it runs no command.
fn clip_paths(text: &str) -> Option<Vec<String>> {
    let ok = |p: &str| p.len() > 1 && p.starts_with('/') && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b));
    text.split(' ').all(ok).then(|| text.split(' ').map(String::from).collect())
}

// The data of a press: each item in order, in chunks. Err = this side cannot go on and the press
// needs a CANCEL. Ok also when the Remote has ended the stream early: its END is in `presses`.
fn feed(c: &C, id: u32, items: &[Item]) -> Result<(), String> {
    let mut buf = vec![0; CHUNK];
    for it in items {
        match &it.src {
            Src::Mem(d) => {
                for chunk in d.chunks(CHUNK) {
                    if !put(c, id, chunk)? {
                        return Ok(());
                    }
                }
            }
            Src::File(p) => {
                let mut f = File::open(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                let mut left = it.size;
                while left > 0 {
                    let n = f.read(&mut buf[..left.min(CHUNK as u64) as usize]).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                    if n == 0 {
                        return Err(format!("{} is shorter than {} B", p.display(), it.size));
                    }
                    if !put(c, id, &buf[..n])? {
                        return Ok(());
                    }
                    left -= n as u64;
                }
            }
        }
    }
    Ok(())
}

// One chunk of data. False when the Remote has ended the stream already.
fn put(c: &C, id: u32, d: &[u8]) -> Result<bool, String> {
    if matches!(c.presses.lock().unwrap().get(&id), Some(Some(_))) {
        return Ok(false);
    }
    if c.out.stream(id, DATA, d) { Ok(true) } else { Err("channel ended".into()) }
}

// The END of a press.
fn press_ended(c: &C, id: u32, p: &[u8]) {
    let f = fields(p);
    if let Some(slot) = c.presses.lock().unwrap().get_mut(&id).filter(|s| s.is_none()) {
        *slot = Some((f[0].parse().unwrap_or(1), f.get(1).cloned().unwrap_or_default()));
        c.press_ended.notify_all();
    } // else: the stream is closed or unknown, and the frame is dropped
}

impl C {
    // The END of press `id`. A channel that ends first gives exit 1.
    fn wait_end(&self, id: u32) -> (u8, String) {
        let mut m = self.presses.lock().unwrap();
        loop {
            if let Some(Some(_)) = m.get(&id) {
                return m.remove(&id).unwrap().unwrap();
            }
            if self.out.dead() {
                m.remove(&id);
                return (1, "channel ended".into());
            }
            m = self.press_ended.wait_timeout(m, Duration::from_secs(1)).unwrap().0;
        }
    }
}
