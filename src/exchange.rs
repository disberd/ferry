// Workstation side of the Exchange folder path. Holds one hidden
// `ssh.exe -T <alias> ~/.local/bin/ferry serve` and reconnects when it ends.
// It answers `ls`, `pull` and `push` from agents in the Exchange folder
// %USERPROFILE%\ferry\.
//
// Only `run` and `connect` need Win32. `channel` and everything below it builds
// on Linux, where the tests run it against `remote::serve_on`.

use crate::frame::*;
#[cfg(windows)]
use crate::windows::{hide, log, sound, ssh};
#[cfg(not(windows))]
use crate::stub::{log, sound};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PREV: &str = ".ferry-prev";

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
    cancelled: Mutex<HashSet<u32>>,
}

#[cfg(windows)]
pub fn run(alias: &str) {
    let remote = "~/.local/bin/ferry serve";
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    let ex = PathBuf::from(std::env::var("USERPROFILE").expect("USERPROFILE is not set")).join("ferry");
    fs::create_dir_all(ex.join(PREV)).unwrap();
    hide(&ex.join(PREV));
    log(&format!("Exchange folder {}, host {host}", ex.display()));

    loop {
        log(&format!("connect: {alias} {remote}"));
        let why = match connect(alias, remote, &host, &ex) {
            Ok(why) => why,
            Err(e) => format!("FAIL: {e}"),
        };
        log(&format!("connection ended: {why}"));
        if let Some(by) = why.strip_prefix("replaced\t") {
            log(&format!("replaced by {by}: exit"));
            std::process::exit(0);
        } else {
            log("reconnect in 5 s");
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

#[cfg(windows)]
fn connect(alias: &str, remote: &str, host: &str, ex: &Path) -> Result<String, String> {
    let mut child = ssh(alias, remote)
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            log(&format!("  remote: {line}"));
        }
    });
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let child: Arc<Mutex<Child>> = Arc::new(Mutex::new(child));
    let ch = child.clone();
    let why = channel(stdin, stdout, ex, host, move || {
        let _ = ch.lock().unwrap().kill();
    });
    let _ = child.lock().unwrap().kill();
    let _ = child.lock().unwrap().wait();
    why
}

// One connection over any byte channel: the pipes of ssh.exe, or a socket pair
// in the tests. `stop` ends the channel when the Remote goes silent.
pub fn channel(mut stdin: impl Write + Send + 'static, r: impl Read, ex: &Path, host: &str, stop: impl Fn() + Send + 'static) -> Result<String, String> {
    let mut r = BufReader::with_capacity(CHUNK + 9, r);
    stdin.write_all(MAGIC).map_err(|e| e.to_string())?;
    let c = Arc::new(C {
        out: Out::default(),
        ex: ex.to_path_buf(),
        rx_ms: AtomicU64::new(now_ms()),
        pushes: Mutex::default(),
        cancelled: Mutex::default(),
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

    let why = session(&c, &mut r);
    c.out.kill();
    for (_, p) in c.pushes.lock().unwrap().drain() {
        drop(p.file);
        let _ = fs::remove_file(&p.tmp);
    }
    Ok(why)
}

fn session(c: &Arc<C>, r: &mut impl BufRead) -> String {
    let mut seen = Vec::new();
    for b in r.by_ref().bytes() {
        match b {
            Ok(b) => seen.push(b),
            Err(e) => return e.to_string(),
        }
        if seen.ends_with(MAGIC) || seen.len() > 4096 + 8 {
            break;
        }
    }
    if !seen.ends_with(MAGIC) {
        return format!("Remote login prints to stdout: {:?}", String::from_utf8_lossy(&seen[..seen.len().min(80)]));
    }
    match read_frame(r) {
        Ok((_, HELLO, p)) if fields(&p)[0] == PROTO => log(&format!("HELLO {:?}: ready", String::from_utf8_lossy(&p))),
        Ok((_, _, p)) => return format!("version mismatch: Workstation {PROTO}, Remote {:?}", String::from_utf8_lossy(&p)),
        Err(e) => return e.to_string(),
    }
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
                    drop(p.file);
                    let _ = fs::remove_file(&p.tmp);
                    log(&format!("{id}: CANCEL, part file deleted"));
                } else {
                    c.cancelled.lock().unwrap().insert(id);
                }
            }
            BYE => return String::from_utf8_lossy(&p).to_string(),
            k => log(&format!("{id}: unknown frame kind {k}")),
        }
    }
}

fn reply(c: &C, id: u32, code: u8, msg: &str) {
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
            || part.starts_with(".ferry-")
        {
            return Err(format!("bad path: {x}"));
        }
        p.push(part);
    }
    Ok(p)
}

fn in_use(e: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, or access denied.
    matches!(e.raw_os_error(), Some(32 | 33)) || e.kind() == io::ErrorKind::PermissionDenied
}

fn open(c: &Arc<C>, id: u32, f: Vec<String>) {
    log(&format!("{id}: OPEN {}", f.join(" ")));
    let path = match resolve(&c.ex, f.get(1).map_or("", |s| s.as_str())) {
        Ok(p) => p,
        Err(e) => return reply(c, id, BAD_PATH, &e),
    };
    let c = c.clone();
    match f[0].as_str() {
        "ls" => {
            std::thread::spawn(move || ls(&c, id, &path));
        }
        "pull" => {
            std::thread::spawn(move || pull(&c, id, &path));
        }
        "push" => {
            let size: u64 = f.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            if size > CAP {
                return reply(&c, id, TOO_LARGE, &format!("{size} B is over the cap of {CAP} B"));
            }
            if path == c.ex {
                return reply(&c, id, BAD_PATH, "push needs a file name");
            }
            let dir = path.parent().unwrap();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if let Err(e) = fs::create_dir_all(dir) {
                return reply(&c, id, 1, &e.to_string());
            }
            let tmp = dir.join(format!(".ferry-part-{id}-{name}"));
            let file = match File::create(&tmp) {
                Ok(f) => f,
                Err(e) => return reply(&c, id, 1, &e.to_string()),
            };
            let push = Push {
                tmp,
                rel: f[1].clone(),
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

fn ls(c: &C, id: u32, dir: &Path) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return reply(c, id, BAD_PATH, &format!("{}: {e}", dir.display())),
    };
    let rel = dir.strip_prefix(&c.ex).unwrap().to_string_lossy().replace('\\', "/");
    let mut lines: Vec<String> = rd
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with(".ferry-"))
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

fn pull(c: &C, id: u32, path: &Path) {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if in_use(&e) => return reply(c, id, IN_USE, &format!("in use: {}", path.display())),
        Err(e) => return reply(c, id, BAD_PATH, &format!("{}: {e}", path.display())),
    };
    let size = f.metadata().unwrap().len();
    if size > CAP {
        return reply(c, id, TOO_LARGE, &format!("{size} B is over the cap of {CAP} B"));
    }
    let t = Instant::now();
    let mut buf = vec![0; CHUNK];
    loop {
        if c.cancelled.lock().unwrap().remove(&id) {
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
    if let Err(e) = p.file.as_mut().unwrap().write_all(d) {
        let p = m.remove(&id).unwrap();
        drop(m);
        drop(p.file);
        let _ = fs::remove_file(&p.tmp);
        return reply(c, id, 1, &e.to_string());
    }
    p.left = p.left.saturating_sub(d.len() as u64);
    if p.left == 0 {
        let p = m.remove(&id).unwrap();
        drop(m);
        finish(c, id, p);
    }
}

fn finish(c: &C, id: u32, mut p: Push) {
    p.file = None; // Windows renames only a closed file
    let secs = p.t.elapsed().as_secs_f64();
    let r = place(c, &p);
    let (code, msg) = match r {
        Ok(m) => (OK, m),
        Err((code, m)) => {
            let _ = fs::remove_file(&p.tmp);
            (code, m)
        }
    };
    reply(c, id, code, &format!("{msg} ({:.1} s)", secs));
    if code == OK {
        sound("Notification.Reminder");
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
            Err(e) if in_use(&e) => return Err((IN_USE, format!("in use: {} ({e})", p.rel))),
            Err(e) => return Err((1, e.to_string())),
        }
    }
    fs::rename(&p.tmp, &p.target).map_err(|e| (1, e.to_string()))?;
    Ok(format!("saved: {}", p.rel))
}
