// Remote side: the agent commands and `serve`, the child of the hidden
// ssh.exe of the Workstation.
// State in ~/.local/state/ferry/ (0700): ferry.sock (0600), serve.lock (flock,
// holds the pid), serve.log, in/ (landing folder).
//
// The helper socket carries the same frames as the channel. Each connection
// starts with the magic and HELLO (protocol, role). A `cli` agent then sends
// OPEN (and DATA for a push) on stream 1. `serve` gives the request an even
// stream id on the channel and copies the replies back.

use crate::frame::*;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

pub fn state() -> PathBuf {
    let d = PathBuf::from(std::env::var("HOME").unwrap()).join(".local/state/ferry");
    for sub in ["", "in"] {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(d.join(sub)).unwrap();
    }
    d
}

// ---- agent commands ----

pub fn base(p: &str) -> &str {
    p.rsplit(['/', '\\']).next().unwrap()
}

// The start of each helper socket connection: the magic, then HELLO with the
// protocol integer and the role of the sender (`cli`, `hook` or `serve`).
pub fn greet(proto: &str, role: &str) -> Vec<u8> {
    [MAGIC.as_slice(), &frame(0, HELLO, format!("{proto}\t{role}").as_bytes())].concat()
}

// The two texts of exit 3 that `serve` can give. `talk` uses the first one for a socket EOF with no END.
const ENDED: &str = "connection to the Workstation ended";
const REPLACED: &str = "replaced by a new connection";

// One frame carries the whole OPEN payload, so a longer request is a bad command line.
fn fits(open: &str) -> bool {
    let ok = open.len() <= CHUNK;
    if !ok {
        eprintln!("{}", crate::USAGE);
    }
    ok
}

// `data` is the file of a push and the size that OPEN declares: exactly that many bytes go out.
// Err(()) = no `serve` listens: the Workstation is offline. A local failure gives Ok with exit 1;
// the socket closes at the return, and `serve` sends CANCEL.
pub fn talk(dir: &Path, open: &str, data: Option<(File, u64)>, out: &mut dyn Write) -> Result<(u8, String), ()> {
    let mut s = UnixStream::connect(dir.join("ferry.sock")).map_err(|_| ())?;
    let _ = s.write_all(&greet(PROTO, "cli"));
    let _ = s.write_all(&frame(1, OPEN, open.as_bytes()));
    if let Some((mut f, mut left)) = data {
        let mut buf = vec![0; CHUNK];
        while left > 0 {
            let n = left.min(CHUNK as u64) as usize;
            if let Err(e) = f.read_exact(&mut buf[..n]) {
                let why = if e.kind() == io::ErrorKind::UnexpectedEof { "the local file got shorter during the push".into() } else { format!("cannot read the local file: {e}") };
                return Ok((1, why));
            }
            if s.write_all(&frame(1, DATA, &buf[..n])).is_err() {
                break; // `serve` went away; the read below gives the reason
            }
            left -= n as u64;
        }
    }
    loop {
        match read_frame(&mut s) {
            Ok((_, DATA, p)) => {
                if let Err(e) = out.write_all(&p) {
                    return Ok((1, format!("cannot write the output: {e}")));
                }
            }
            Ok((_, END, p)) => {
                let f = fields(&p);
                return Ok((f[0].parse().unwrap_or(1), f.get(1).cloned().unwrap_or_default()));
            }
            Ok(_) => {}
            Err(_) => return Ok((OFFLINE, ENDED.into())),
        }
    }
}

pub fn ls(d: &str) -> i32 {
    let open = format!("ls\t{d}");
    if !fits(&open) {
        return BAD_PATH as i32;
    }
    show(talk(&state(), &open, None, &mut io::stdout()), false)
}

// `say`: print the text of an END 0 (`push` has one; the others print their data only).
fn show(r: Result<(u8, String), ()>, say: bool) -> i32 {
    match r {
        Ok((OK, m)) => {
            if say && !m.is_empty() {
                println!("{m}");
            }
            0
        }
        Ok((c, m)) => {
            eprintln!("ferry: {m}");
            c as i32
        }
        Err(()) => {
            eprintln!("ferry: Workstation offline");
            OFFLINE as i32
        }
    }
}

pub fn pull(x: &str, dest: Option<&str>) -> i32 {
    pull_in(&state(), x, dest)
}

// `pull` with the state folder `dir`.
pub fn pull_in(dir: &Path, x: &str, dest: Option<&str>) -> i32 {
    let open = format!("pull\t{x}");
    if !fits(&open) {
        return BAD_PATH as i32;
    }
    if dest == Some("-") {
        return show(talk(dir, &open, None, &mut io::stdout()), false);
    }
    pull_file(dir, &open, x, dest).unwrap_or_else(|e| {
        eprintln!("ferry: {e}");
        1
    })
}

// Pull into a file. `dir` is the state folder. Err = a local file that ferry cannot write; the part file is gone.
fn pull_file(dir: &Path, open: &str, x: &str, dest: Option<&str>) -> Result<i32, String> {
    let path = match dest {
        Some(d) => PathBuf::from(d),
        None => {
            let entry = landing(&dir.join("in")).map_err(|e| format!("Landing folder: {e}"))?;
            entry.join(safe_name(base(x)))
        }
    };
    let part = path.with_extension("ferry-part");
    let mut f = File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let r = talk(dir, open, None, &mut f);
    if matches!(r, Ok((OK, _))) {
        if let Err(e) = fs::rename(&part, &path) {
            let _ = fs::remove_file(&part);
            return Err(format!("{}: {e}", path.display()));
        }
        println!("{}", path.display());
        return Ok(0);
    }
    let _ = fs::remove_file(&part);
    Ok(show(r, false))
}

pub fn push(local: &str, x: &str) -> i32 {
    let opened = match File::open(local).and_then(|f| f.metadata().map(|m| (f, m))) {
        Ok((_, m)) if m.is_dir() => Err("is a folder".to_string()),
        Ok((f, m)) => Ok((f, m.len())),
        Err(e) => Err(e.to_string()),
    };
    let (f, size) = match opened {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ferry: {local}: {e}");
            return 1;
        }
    };
    let open = format!("push\t{x}\t{size}");
    if !fits(&open) {
        return BAD_PATH as i32;
    }
    show(talk(&state(), &open, Some((f, size)), &mut io::sink()), true)
}

// ---- Landing folder ----

// Each entry of `in/` older than this is deleted when a new entry starts.
const KEEP: Duration = Duration::from_secs(7 * 86400);

// A new entry `<inn>/<YYYYmmdd-HHMMSS>` (UTC, mode 0700). A name that exists gets `-2`, `-3`, and so on.
// It also deletes the entries that are older than `KEEP`.
fn landing(inn: &Path) -> io::Result<PathBuf> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(inn)?;
    for e in fs::read_dir(inn)?.filter_map(Result::ok) {
        if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > KEEP)) {
            let _ = fs::remove_dir_all(e.path());
        }
    }
    let [y, mo, d, h, mi, s] = crate::lifecycle::utc(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |t| t.as_secs()) as i64);
    let stamp = format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}");
    let mut n = 1;
    loop {
        let entry = inn.join(if n == 1 { stamp.clone() } else { format!("{stamp}-{n}") });
        match fs::DirBuilder::new().mode(0o700).create(&entry) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
            r => return r.map(|()| entry),
        }
    }
}

// Keeps [A-Za-z0-9._-]; every other character becomes `_`, so a pasted path needs no quotes.
// A name with no other character (`..` too) becomes `_`. A name over `NAME_MAX` loses the end
// of its stem, so that `-100` and `.ferry-part` still fit in the 255 bytes of a file name.
fn safe_name(name: &str) -> String {
    const NAME_MAX: usize = 240;
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    if s.trim_matches('.').is_empty() {
        return "_".into();
    }
    let (stem, ext) = s.split_at(s.rfind('.').filter(|&i| i > 0 && s.len() - i <= 16).unwrap_or(s.len()));
    if s.len() > NAME_MAX { format!("{}{ext}", &stem[..NAME_MAX - ext.len()]) } else { s }
}

// `name`, else `-2`, `-3`, and so on before the extension, until `taken` gives false.
fn unique(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = name.split_at(name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len()));
    let mut n = 1;
    loop {
        let c = if n == 1 { name.to_string() } else { format!("{stem}-{n}{ext}") };
        if !taken(&c) {
            return c;
        }
        n += 1;
    }
}

// ---- serve ----

struct S {
    out: Out,
    peers: Mutex<HashMap<u32, UnixStream>>,
    next: AtomicU32,
    rx_ms: AtomicU64,
    log: Mutex<File>,
    echo: bool,
    done: Mutex<bool>,
    ended: Condvar,
    // `in/`, and the hotkey presses that the Workstation streams (odd stream ids).
    inn: PathBuf,
    presses: Mutex<HashMap<u32, Press>>,
}

impl S {
    // Only `serve` logs; the agent commands print to stderr themselves. File
    // first: stderr is the ssh pipe, and it can be closed already.
    fn log(&self, msg: &str) {
        let line = format!("{:.3} serve[{}]: {msg}\n", now_ms() as f64 / 1000.0, std::process::id());
        let _ = self.log.lock().unwrap().write_all(line.as_bytes());
        if self.echo {
            let _ = io::stderr().write_all(line.as_bytes());
        }
    }

    // One frame for the agent of stream `id`; an END also closes the stream. When the write fails
    // (a timeout included), the frame may be half written: close the socket, and cancel the stream
    // on the channel unless the frame was the END.
    fn to_agent(&self, id: u32, kind: u8, p: &[u8]) {
        let mut peers = self.peers.lock().unwrap();
        let Some(c) = peers.get_mut(&id) else { return };
        let r = c.write_all(&frame(id, kind, p));
        if r.is_ok() && kind != END {
            return;
        }
        let c = peers.remove(&id).unwrap();
        drop(peers);
        if let Err(e) = r {
            let _ = c.shutdown(Shutdown::Both);
            if kind != END {
                self.out.ctrl(id, CANCEL, b"");
            }
            self.log(&format!("stream {id}: agent does not read ({e}), dropped"));
        }
    }

    fn finished(&self) -> bool {
        *self.done.lock().unwrap()
    }
}

// Stop all open requests with END 3 and the text `said`, then end `serve`. The flock goes when
// `serve` returns and the process exits.
fn quit(s: &S, why: &str, said: &str) {
    s.log(&format!("exit: {why}"));
    // One thread for each agent: stuck agents cost 1 s together, not 1 s each.
    let open: Vec<_> = s.peers.lock().unwrap().drain().collect();
    std::thread::scope(|t| {
        for (id, mut c) in open {
            t.spawn(move || {
                if c.write_all(&frame(id, END, &end(OFFLINE, said))).is_err() {
                    let _ = c.shutdown(Shutdown::Both);
                }
            });
        }
    });
    for (_, p) in s.presses.lock().unwrap().drain() {
        let _ = fs::remove_dir_all(p.dir);
    }
    *s.done.lock().unwrap() = true;
    s.ended.notify_all();
}

// Takes `serve.lock` and writes the pid into it. When an older `serve` holds
// the lock, this asks it to quit over the helper socket (TAKEOVER) and waits
// `wait` for the lock. If the lock stays held, `kill(pid)` ends the holder.
// A failed connect means a crash left the socket behind; the caller deletes it.
fn take_lock(dir: &Path, host: &str, wait: Duration, log: &dyn Fn(&str), kill: &dyn Fn(&str)) -> File {
    let lockf = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("serve.lock")).unwrap();
    if lockf.try_lock().is_err() {
        let t = std::time::Instant::now();
        match UnixStream::connect(dir.join("ferry.sock")) {
            Ok(mut c) => {
                let _ = c.write_all(&greet(PROTO, "serve"));
                let _ = c.write_all(&frame(0, TAKEOVER, host.as_bytes()));
                log("TAKEOVER sent");
            }
            Err(e) => log(&format!("lock held but socket refuses ({e})")),
        }
        while lockf.try_lock().is_err() && t.elapsed() < wait {
            std::thread::sleep(Duration::from_millis(50));
        }
        if lockf.try_lock().is_err() {
            let pid = fs::read_to_string(dir.join("serve.lock")).unwrap_or_default();
            log(&format!("old serve {} still holds the lock after {} s: SIGKILL", pid.trim(), wait.as_secs()));
            kill(pid.trim());
            lockf.lock().unwrap();
        }
        log(&format!("lock taken after {} ms", t.elapsed().as_millis()));
    }
    lockf.set_len(0).unwrap();
    (&lockf).write_all(std::process::id().to_string().as_bytes()).unwrap();
    lockf
}

// `serve` on the stdio of the ssh child. `serve_on` has the details.
pub fn serve() -> i32 {
    let d = state();
    let log = d.join("serve.log");
    crate::lifecycle::rotate(&log);
    crate::lifecycle::panic_hook(move |m| crate::lifecycle::append(&log, &format!("{:.3} serve[{}]: {m}\n", now_ms() as f64 / 1000.0, std::process::id())));
    serve_on(&d, io::stdin(), io::stdout(), true)
}

// Runs one channel: `r` and `w` are the pipes to the Workstation and `dir` is
// the state folder (it must exist). Returns when the channel or the
// replacement ends it. `echo` copies the log lines to stderr.
pub fn serve_on(dir: &Path, r: impl Read + Send + 'static, mut w: impl Write + Send + 'static, echo: bool) -> i32 {
    let me = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();
    let s = Arc::new(S {
        out: Out::default(),
        peers: Mutex::default(),
        next: AtomicU32::new(2),
        rx_ms: AtomicU64::new(now_ms()),
        log: Mutex::new(OpenOptions::new().create(true).append(true).open(dir.join("serve.log")).unwrap()),
        echo,
        done: Mutex::new(false),
        ended: Condvar::new(),
        inn: dir.join("in"),
        presses: Mutex::default(),
    });

    // Handshake. The Workstation skips login noise up to MAGIC.
    w.write_all(MAGIC).unwrap();
    w.write_all(&frame(0, HELLO, format!("{PROTO}\t{VERSION}\t{me}").as_bytes())).unwrap();
    w.flush().unwrap();
    let mut stdin = BufReader::with_capacity(CHUNK + 9, r);
    let mut m = [0u8; 8];
    stdin.read_exact(&mut m).unwrap();
    assert_eq!(&m, MAGIC, "no magic from the Workstation");
    let (_, kind, p) = read_frame(&mut stdin).unwrap();
    let hello = fields(&p);
    assert_eq!(kind, HELLO);
    if hello[0] != PROTO {
        s.log(&format!("version mismatch: Workstation {}, Remote {PROTO}", hello[0]));
        return 1;
    }
    let host = hello.get(2).cloned().unwrap_or_default();
    s.log(&format!("HELLO from {host}"));

    // Replacement: take the lock from an older `serve`.
    let _lockf = take_lock(dir, &host, Duration::from_secs(5), &|m| s.log(m), &|pid| {
        let _ = std::process::Command::new("kill").args(["-9", pid]).status();
    });
    let sock = dir.join("ferry.sock");
    let _ = fs::remove_file(&sock); // a stale socket from a crash, or none
    let listener = UnixListener::bind(&sock).unwrap();
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600)).unwrap();

    s.rx_ms.store(now_ms(), SeqCst);
    let s2 = s.clone();
    std::thread::spawn(move || s2.out.run(w));
    let s2 = s.clone();
    std::thread::spawn(move || from_workstation(&s2, stdin));
    let s2 = s.clone();
    std::thread::spawn(move || {
        while !s2.finished() {
            std::thread::sleep(Duration::from_secs(PING_S));
            s2.out.ctrl(0, PING, b"");
        }
    });
    let s2 = s.clone();
    std::thread::spawn(move || {
        while !s2.finished() {
            std::thread::sleep(Duration::from_secs(1));
            let silent = now_ms().saturating_sub(s2.rx_ms.load(SeqCst));
            if silent > DEAD_MS {
                quit(&s2, &format!("Workstation silent for {} s", silent / 1000), ENDED);
            }
        }
    });
    s.log(&format!("ready, {}", sock.display()));
    let s2 = s.clone();
    std::thread::spawn(move || {
        for c in listener.incoming().map_while(Result::ok) {
            let s3 = s2.clone();
            std::thread::spawn(move || from_agent(&s3, c));
        }
    });
    let _ended = s.ended.wait_while(s.done.lock().unwrap(), |d| !*d).unwrap();
    0
}

fn from_workstation(s: &S, mut r: impl Read) {
    loop {
        let (id, kind, p) = match read_frame(&mut r) {
            Ok(f) => f,
            Err(e) => return quit(s, &format!("channel: {e}"), ENDED),
        };
        s.rx_ms.store(now_ms(), SeqCst);
        match kind {
            PING => {}
            OPEN if id % 2 == 1 => press_open(s, id, &p),
            // The Workstation opens the odd streams. The data of a press goes to its files; `serve` sends the END.
            DATA if id % 2 == 1 => press_data(s, id, &p),
            // A slow agent stalls this reader for 1 s at most: `to_agent` drops it then.
            DATA | END => s.to_agent(id, kind, &p),
            // A CANCEL closes a press of the Workstation: its Landing folder entry goes. For other ids it is dropped.
            CANCEL => press_cancel(s, id),
            BYE => return quit(s, &format!("BYE {}", String::from_utf8_lossy(&p)), ENDED),
            k => s.log(&format!("unknown frame kind {k} on {id}")),
        }
    }
}

fn from_agent(s: &S, mut c: UnixStream) {
    // The timeout belongs to the socket, so it covers each clone of `c` too.
    let _ = c.set_write_timeout(Some(Duration::from_secs(1)));
    let mut m = [0u8; 8];
    if c.read_exact(&mut m).is_err() || m != *MAGIC {
        return;
    }
    let Ok((_, HELLO, p)) = read_frame(&mut c) else { return };
    let f = fields(&p);
    let role = f.get(1).map_or("", String::as_str);
    if f[0] != PROTO {
        let why = format!("ferry serve speaks protocol {PROTO}, this ferry {}", f[0]);
        if role == "hook" {
            s.log(&why); // a hook reads no reply
        } else {
            let _ = c.write_all(&frame(0, END, &end(1, &why)));
        }
        return;
    }
    let Ok((_, kind, p)) = read_frame(&mut c) else { return };
    match (role, kind) {
        ("serve", TAKEOVER) => {
            let host = String::from_utf8_lossy(&p).to_string();
            s.out.ctrl(0, BYE, format!("replaced\t{host}").as_bytes());
            std::thread::sleep(Duration::from_millis(300)); // let the writer send BYE
            return quit(s, "replaced by a new connection", REPLACED);
        }
        ("cli", OPEN) => {}
        _ => return,
    }
    let open = p;
    let id = s.next.fetch_add(2, SeqCst);
    let t = std::time::Instant::now();
    s.peers.lock().unwrap().insert(id, c.try_clone().unwrap());
    s.out.ctrl(id, OPEN, &open);
    s.log(&format!("stream {id}: {}", String::from_utf8_lossy(&open).replace('\t', " ")));
    loop {
        match read_frame(&mut c) {
            Ok((1, DATA, d)) => {
                let live = s.peers.lock().unwrap().contains_key(&id);
                if live {
                    s.out.stream(id, DATA, &d);
                }
            }
            Ok(_) => {}
            Err(_) => {
                if s.peers.lock().unwrap().remove(&id).is_some() {
                    s.out.ctrl(id, CANCEL, b"");
                    s.log(&format!("stream {id}: agent left, CANCEL"));
                } else {
                    s.log(&format!("stream {id}: closed after {} ms", t.elapsed().as_millis()));
                }
                return;
            }
        }
    }
}

// ---- hotkey press ----

// One press of the Workstation. Its files go into one Landing folder entry, one after the
// other, as the declared sizes split the data.
struct Press {
    dir: PathBuf,
    // Final name and size of each file, in order.
    files: Vec<(String, u64)>,
    // The files in place. The next file gets the bytes, and `file` is its open part file.
    done: usize,
    file: Option<File>,
    // Bytes that the open file, and all files, still need.
    left: u64,
    total: u64,
}

// The stream ends with this END, and `serve` sends no more frames for it.
fn press_end(s: &S, id: u32, code: u8, msg: &str) {
    s.log(&format!("press {id}: END {code} {msg}"));
    s.out.stream(id, END, &end(code, msg));
}

// `OPEN hotkey <count> <name> <size> ...`
fn press_open(s: &S, id: u32, p: &[u8]) {
    let f = fields(p);
    let n = f.get(1).and_then(|n| n.parse::<usize>().ok()).filter(|&n| n > 0 && f.len() == 2 + 2 * n);
    let (Some(n), "hotkey") = (n, f[0].as_str()) else { return press_end(s, id, 1, "bad hotkey request") };
    let mut files = Vec::new();
    let mut total = 0u64;
    for i in 0..n {
        let Ok(size) = f[3 + 2 * i].parse::<u64>() else { return press_end(s, id, 1, "bad hotkey request") };
        total = total.saturating_add(size);
        files.push((f[2 + 2 * i].clone(), size));
    }
    if total > CAP {
        return press_end(s, id, TOO_LARGE, &format!("{total} B is over the cap of {CAP} B"));
    }
    let dir = match landing(&s.inn) {
        Ok(d) => d,
        Err(e) => return press_end(s, id, 1, &format!("Landing folder: {e}")),
    };
    let mut names: Vec<String> = Vec::new();
    for (name, _) in &mut files {
        *name = unique(&safe_name(name), |c| names.iter().any(|x| x == c));
        names.push(name.clone());
    }
    s.log(&format!("press {id}: {n} files, {total} B, {}", dir.display()));
    let mut p = Press { dir, files, done: 0, file: None, left: 0, total };
    match advance(&mut p) {
        Ok(true) => press_done(s, id, &p),
        Ok(false) => {
            s.presses.lock().unwrap().insert(id, p);
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&p.dir);
            press_end(s, id, 1, &e.to_string());
        }
    }
}

fn press_data(s: &S, id: u32, d: &[u8]) {
    let mut m = s.presses.lock().unwrap();
    let Some(p) = m.get_mut(&id) else { return };
    let r = if d.len() as u64 > p.total { Err("more data than declared".to_string()) } else { store(p, d).map_err(|e| e.to_string()) };
    match r {
        Ok(false) => {}
        Ok(true) => {
            let p = m.remove(&id).unwrap();
            drop(m);
            press_done(s, id, &p);
        }
        Err(e) => {
            let p = m.remove(&id).unwrap();
            drop(m);
            let _ = fs::remove_dir_all(&p.dir);
            press_end(s, id, 1, &e);
        }
    }
}

// The Workstation gave up: the entry goes, and nothing is pasted.
fn press_cancel(s: &S, id: u32) {
    let p = s.presses.lock().unwrap().remove(&id);
    if let Some(p) = p {
        let _ = fs::remove_dir_all(&p.dir);
        s.log(&format!("press {id}: CANCEL, entry deleted"));
    }
}

// All files are in place. The paths go back for the clipboard of the Workstation. An attempt to
// type them into the focused herdr pane goes here, before this reply.
fn press_done(s: &S, id: u32, p: &Press) {
    let paths: Vec<String> = p.files.iter().map(|(name, _)| p.dir.join(name).display().to_string()).collect();
    press_end(s, id, OK, &format!("clip {}", paths.join(" ")));
}

// Writes `d` into the files. Ok(true) when the last file is in place.
fn store(p: &mut Press, mut d: &[u8]) -> io::Result<bool> {
    p.total -= d.len() as u64;
    while !d.is_empty() {
        let (now, rest) = d.split_at((d.len() as u64).min(p.left) as usize);
        p.file.as_mut().unwrap().write_all(now)?;
        p.left -= now.len() as u64;
        d = rest;
        advance(p)?;
    }
    Ok(p.done == p.files.len())
}

// Renames the part file when it is complete, then opens the part file of the next file. A file
// of size 0 passes at once. Ok(true) when all files are in place.
fn advance(p: &mut Press) -> io::Result<bool> {
    loop {
        if p.file.is_some() && p.left == 0 {
            p.file = None;
            let name = &p.files[p.done].0;
            fs::rename(p.dir.join(format!("{name}.ferry-part")), p.dir.join(name))?;
            p.done += 1;
        }
        let Some((name, size)) = p.files.get(p.done) else { return Ok(true) };
        if p.file.is_some() {
            return Ok(false);
        }
        p.file = Some(File::create(p.dir.join(format!("{name}.ferry-part")))?);
        p.left = *size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // A folder with `serve.lock` held by this test, as an older `serve` would.
    fn held_lock(name: &str) -> (PathBuf, File) {
        let d = std::env::temp_dir().join(format!("ferry-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let held = OpenOptions::new().create(true).truncate(false).write(true).open(d.join("serve.lock")).unwrap();
        held.lock().unwrap();
        (&held).write_all(b"4242").unwrap();
        (d, held)
    }

    // The holder ignores TAKEOVER (here the socket is stale, so the connect fails):
    // after the wait, the injected `kill` gets the pid from the lock file. No real process gets a signal.
    #[test]
    fn a_holder_that_stays_is_killed_after_the_wait() {
        let (d, held) = held_lock("kill");
        drop(UnixListener::bind(d.join("ferry.sock")).unwrap());
        let killed = RefCell::new(None);
        let lockf = take_lock(&d, "h", Duration::from_millis(100), &|_| {}, &|pid| {
            *killed.borrow_mut() = Some(pid.to_string());
            held.unlock().unwrap();
        });
        assert_eq!(killed.into_inner().as_deref(), Some("4242"));
        drop(lockf);
        assert_eq!(fs::read_to_string(d.join("serve.lock")).unwrap(), std::process::id().to_string());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_holder_that_quits_in_time_is_not_killed() {
        let (d, held) = held_lock("quit");
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            held.unlock().unwrap();
        });
        take_lock(&d, "h", Duration::from_secs(5), &|_| {}, &|_| panic!("killed a holder that quit"));
        fs::remove_dir_all(d).unwrap();
    }
}
