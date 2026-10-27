// Remote side: the agent commands and `serve`, the child of the hidden
// ssh.exe of the Workstation.
// State in ~/.local/state/ferry/ (0700): serve.sock (0600), serve.lock (flock,
// holds the pid), serve.log, in/ (landing folder).
//
// The agent socket carries the same frames as the channel. An agent sends OPEN
// (and DATA for a push) on stream 1. `serve` gives the request an even stream
// id on the channel and copies the replies back.

use crate::frame::*;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

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

// Err(()) = no `serve` listens: the Workstation is offline.
fn talk(open: &str, data: Option<File>, out: &mut dyn Write) -> Result<(u8, String), ()> {
    let mut s = UnixStream::connect(state().join("serve.sock")).map_err(|_| ())?;
    let _ = s.write_all(&frame(1, OPEN, open.as_bytes()));
    if let Some(mut f) = data {
        let mut buf = vec![0; CHUNK];
        while let Ok(n @ 1..) = f.read(&mut buf) {
            if s.write_all(&frame(1, DATA, &buf[..n])).is_err() {
                break; // `serve` went away; the read below gives the reason
            }
        }
    }
    loop {
        match read_frame(&mut s) {
            Ok((_, DATA, p)) => out.write_all(&p).map_err(|_| ())?,
            Ok((_, END, p)) => {
                let f = fields(&p);
                return Ok((f[0].parse().unwrap_or(1), f.get(1).cloned().unwrap_or_default()));
            }
            Ok(_) => {}
            Err(_) => return Ok((OFFLINE, "connection to the Workstation ended".into())),
        }
    }
}

pub fn ls(d: &str) -> i32 {
    show(talk(&format!("ls\t{d}"), None, &mut io::stdout()))
}

fn show(r: Result<(u8, String), ()>) -> i32 {
    match r {
        Ok((OK, m)) if m.is_empty() => 0,
        Ok((OK, m)) => {
            println!("{m}");
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
    if dest == Some("-") {
        return show(talk(&format!("pull\t{x}"), None, &mut io::stdout()));
    }
    let path = match dest {
        Some(d) => PathBuf::from(d),
        None => {
            let dir = state().join("in").join((now_ms() / 1000).to_string());
            fs::create_dir_all(&dir).unwrap();
            let name: String =
                base(x).chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
            dir.join(name)
        }
    };
    let part = path.with_extension("ferry-part");
    let r = talk(&format!("pull\t{x}"), None, &mut File::create(&part).unwrap());
    if matches!(r, Ok((OK, _))) {
        fs::rename(&part, &path).unwrap();
        println!("{}", path.display());
        return 0;
    }
    let _ = fs::remove_file(&part);
    show(r)
}

pub fn push(local: &str, x: &str) -> i32 {
    let f = match File::open(local) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("ferry: {local}: {e}");
            return 1;
        }
    };
    let size = f.metadata().unwrap().len();
    show(talk(&format!("push\t{x}\t{size}"), Some(f), &mut io::sink()))
}

// ---- serve ----

struct S {
    out: Out,
    peers: Mutex<HashMap<u32, UnixStream>>,
    next: AtomicU32,
    rx_ms: AtomicU64,
}

static SV: LazyLock<S> = LazyLock::new(|| S {
    out: Out::default(),
    peers: Mutex::default(),
    next: AtomicU32::new(2),
    rx_ms: AtomicU64::new(now_ms()),
});

static LOG: LazyLock<Mutex<File>> = LazyLock::new(|| {
    Mutex::new(OpenOptions::new().create(true).append(true).open(state().join("serve.log")).unwrap())
});

// Only `serve` logs; the agent commands print to stderr themselves. File
// first: stderr is the ssh pipe, and it can be closed already.
fn log(msg: &str) {
    let line = format!("{:.3} serve[{}]: {msg}\n", now_ms() as f64 / 1000.0, std::process::id());
    let _ = LOG.lock().unwrap().write_all(line.as_bytes());
    let _ = io::stderr().write_all(line.as_bytes());
}

// Stop all open requests with exit 3, then exit. flock goes with the process.
fn quit(s: &S, why: &str) -> ! {
    log(&format!("exit: {why}"));
    for (id, mut c) in s.peers.lock().unwrap().drain() {
        let _ = c.write_all(&frame(id, END, &end(OFFLINE, why)));
    }
    std::process::exit(0)
}

pub fn serve() -> i32 {
    let dir = state();
    let me = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();

    // Handshake. The Workstation skips login noise up to MAGIC.
    let mut stdout = io::stdout();
    stdout.write_all(MAGIC).unwrap();
    stdout.write_all(&frame(0, HELLO, format!("{PROTO}\t{VERSION}\t{me}").as_bytes())).unwrap();
    stdout.flush().unwrap();
    let mut stdin = BufReader::with_capacity(CHUNK + 9, io::stdin());
    let mut m = [0u8; 8];
    stdin.read_exact(&mut m).unwrap();
    assert_eq!(&m, MAGIC, "no magic from the Workstation");
    let (_, kind, p) = read_frame(&mut stdin).unwrap();
    let hello = fields(&p);
    assert_eq!(kind, HELLO);
    if hello[0] != PROTO {
        log(&format!("version mismatch: Workstation {}, Remote {PROTO}", hello[0]));
        return 1;
    }
    let host = hello.get(2).cloned().unwrap_or_default();
    log(&format!("HELLO from {host}"));

    // Replacement: take the lock from an older `serve`.
    let lockf = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("serve.lock")).unwrap();
    if lockf.try_lock().is_err() {
        let t = std::time::Instant::now();
        match UnixStream::connect(dir.join("serve.sock")) {
            Ok(mut c) => {
                let _ = c.write_all(&frame(0, TAKEOVER, host.as_bytes()));
                log("TAKEOVER sent");
            }
            Err(e) => log(&format!("lock held but socket refuses ({e})")),
        }
        while lockf.try_lock().is_err() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if lockf.try_lock().is_err() {
            let pid = fs::read_to_string(dir.join("serve.lock")).unwrap_or_default();
            log(&format!("old serve {} still holds the lock after 5 s: SIGKILL", pid.trim()));
            let _ = std::process::Command::new("kill").args(["-9", pid.trim()]).status();
            lockf.lock().unwrap();
        }
        log(&format!("lock taken after {} ms", t.elapsed().as_millis()));
    }
    lockf.set_len(0).unwrap();
    (&lockf).write_all(std::process::id().to_string().as_bytes()).unwrap();
    let sock = dir.join("serve.sock");
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600)).unwrap();

    let s: &'static S = &SV;
    s.rx_ms.store(now_ms(), SeqCst);
    std::thread::spawn(move || s.out.run(io::stdout().lock()));
    std::thread::spawn(move || from_workstation(s, stdin));
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(PING_S));
        s.out.ctrl(0, PING, b"");
    });
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let silent = now_ms().saturating_sub(s.rx_ms.load(SeqCst));
        if silent > DEAD_MS {
            quit(s, &format!("Workstation silent for {} s", silent / 1000));
        }
    });
    log(&format!("ready, {}", sock.display()));
    for c in listener.incoming().map_while(Result::ok) {
        std::thread::spawn(move || from_agent(s, c));
    }
    0
}

fn from_workstation(s: &'static S, mut r: impl Read) {
    loop {
        let (id, kind, p) = match read_frame(&mut r) {
            Ok(f) => f,
            Err(e) => quit(s, &format!("channel: {e}")),
        };
        s.rx_ms.store(now_ms(), SeqCst);
        match kind {
            PING => {}
            DATA => {
                // ponytail: a slow agent stalls this reader; give each agent a queue if it matters.
                if let Some(c) = s.peers.lock().unwrap().get_mut(&id) {
                    let _ = c.write_all(&frame(id, DATA, &p));
                }
            }
            END => {
                if let Some(mut c) = s.peers.lock().unwrap().remove(&id) {
                    let _ = c.write_all(&frame(id, END, &p));
                }
            }
            BYE => quit(s, &format!("BYE {}", String::from_utf8_lossy(&p))),
            k => log(&format!("unknown frame kind {k} on {id}")),
        }
    }
}

fn from_agent(s: &'static S, mut c: UnixStream) {
    let Ok((_, kind, p)) = read_frame(&mut c) else { return };
    if kind == TAKEOVER {
        let host = String::from_utf8_lossy(&p).to_string();
        s.out.ctrl(0, BYE, format!("replaced\t{host}").as_bytes());
        std::thread::sleep(Duration::from_millis(300)); // let the writer send BYE
        quit(s, "replaced by a new connection");
    }
    if kind != OPEN {
        return;
    }
    let open = p;
    let id = s.next.fetch_add(2, SeqCst);
    let t = std::time::Instant::now();
    s.peers.lock().unwrap().insert(id, c.try_clone().unwrap());
    s.out.ctrl(id, OPEN, &open);
    log(&format!("stream {id}: {}", String::from_utf8_lossy(&open).replace('\t', " ")));
    loop {
        match read_frame(&mut c) {
            Ok((_, DATA, d)) => {
                let live = s.peers.lock().unwrap().contains_key(&id);
                if live {
                    s.out.stream(id, DATA, &d);
                }
            }
            Ok(_) => {}
            Err(_) => {
                if s.peers.lock().unwrap().remove(&id).is_some() {
                    s.out.ctrl(id, CANCEL, b"");
                    log(&format!("stream {id}: agent left, CANCEL"));
                } else {
                    log(&format!("stream {id}: closed after {} ms", t.elapsed().as_millis()));
                }
                return;
            }
        }
    }
}
