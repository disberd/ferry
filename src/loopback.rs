// Loopback tests: the Workstation core (`exchange::channel`) and `remote::serve_on`
// run in this process, joined by one socket pair, with a temporary Exchange
// folder and a temporary Remote state folder. The agent side is `remote::talk`
// on the helper socket of the state folder.

use crate::frame::*;
use crate::{exchange, remote};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type Reply = (u8, String);

struct Loop {
    root: PathBuf,
    ex: PathBuf,
    state: PathBuf,
    channel: UnixStream,
    serve: Option<JoinHandle<i32>>,
    ws: Option<JoinHandle<Result<String, String>>>,
}

// The Workstation core and one `serve` on a socket pair.
fn pair(state: &Path, ex: &Path, host: &'static str) -> (UnixStream, JoinHandle<i32>, JoinHandle<Result<String, String>>) {
    let (a, b) = UnixStream::pair().unwrap();
    let channel = a.try_clone().unwrap();
    let (st, ex) = (state.to_path_buf(), ex.to_path_buf());
    let b2 = b.try_clone().unwrap();
    let serve = std::thread::spawn(move || remote::serve_on(&st, b, b2, false));
    let a2 = a.try_clone().unwrap();
    let ws = std::thread::spawn(move || exchange::channel(a2, a, &ex, host, || {}));
    (channel, serve, ws)
}

impl Loop {
    fn start() -> Loop {
        Loop::start_with(|_| {})
    }

    // `prep` runs on the state folder before `serve` starts.
    fn start_with(prep: impl FnOnce(&Path)) -> Loop {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!("ferry-test-{}-{}", std::process::id(), N.fetch_add(1, SeqCst)));
        let (ex, state) = (root.join("ex"), root.join("state"));
        fs::create_dir_all(&ex).unwrap();
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&state).unwrap();
        prep(&state);
        let (channel, serve, ws) = pair(&state, &ex, "test-host");
        // A stale socket file can exist already, so wait for a connect, not for the file.
        let t = Instant::now();
        while UnixStream::connect(state.join("ferry.sock")).is_err() {
            assert!(t.elapsed() < Duration::from_secs(5), "serve did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        Loop { root, ex, state, channel, serve: Some(serve), ws: Some(ws) }
    }

    // Close the channel. Both sides must end; returns the exit code of `serve`.
    fn stop(&mut self) -> Option<i32> {
        let _ = self.channel.shutdown(std::net::Shutdown::Both);
        let _ = self.ws.take()?.join();
        self.serve.take()?.join().ok()
    }

    fn talk(&self, open: &str, data: Option<&[u8]>) -> (Result<Reply, ()>, Vec<u8>) {
        let local = data.map(|d| {
            let p = self.root.join("local");
            fs::write(&p, d).unwrap();
            fs::File::open(p).unwrap()
        });
        let mut out = Vec::new();
        (remote::talk(&self.state, open, local, &mut out), out)
    }

    fn ls(&self, d: &str) -> (u8, String) {
        let (r, out) = self.talk(&format!("ls\t{d}"), None);
        (r.unwrap().0, String::from_utf8(out).unwrap())
    }

    fn push(&self, x: &str, data: &[u8]) -> Reply {
        self.talk(&format!("push\t{x}\t{}", data.len()), Some(data)).0.unwrap()
    }

    fn pull(&self, x: &str) -> (u8, Vec<u8>) {
        let (r, out) = self.talk(&format!("pull\t{x}"), None);
        (r.unwrap().0, out)
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.root);
    }
}

// Spans several 64 KiB frames, so a chunk border sits inside the file.
fn bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

fn leftovers(l: &Loop) -> Vec<String> {
    let mut v = Vec::new();
    let mut dirs = vec![l.ex.clone()];
    while let Some(d) = dirs.pop() {
        for e in fs::read_dir(d).unwrap().map(Result::unwrap) {
            if e.file_name().to_string_lossy().starts_with(".ferry-part") {
                v.push(e.path().display().to_string());
            }
            if e.path().is_dir() {
                dirs.push(e.path());
            }
        }
    }
    v
}

#[test]
fn ls_push_pull() {
    let l = Loop::start();
    assert_eq!(l.ls(""), (OK, String::new()));

    let big = bytes(3 * CHUNK + 123);
    let (code, msg) = l.push("sub/big.bin", &big);
    assert_eq!(code, OK, "{msg}");
    assert_eq!(fs::read(l.ex.join("sub/big.bin")).unwrap(), big);
    assert!(leftovers(&l).is_empty(), "part file left: {:?}", leftovers(&l));

    let (code, listing) = l.ls("");
    assert_eq!(code, OK);
    let row: Vec<&str> = listing.trim_end().split('\t').collect();
    assert_eq!(row[2], "sub/", "{listing}");
    let (code, listing) = l.ls("sub");
    let row: Vec<&str> = listing.trim_end().split('\t').collect();
    assert_eq!((code, row[0], row[2]), (OK, big.len().to_string().as_str(), "sub/big.bin"), "{listing}");

    assert_eq!(l.pull("sub/big.bin"), (OK, big));
    // Backslashes work as separators in an agent path.
    assert_eq!(l.pull("sub\\big.bin").0, OK);
}

#[test]
fn empty_file_round_trip() {
    let l = Loop::start();
    assert_eq!(l.push("empty.txt", b"").0, OK);
    assert_eq!(fs::read(l.ex.join("empty.txt")).unwrap(), b"");
    assert_eq!(l.pull("empty.txt"), (OK, Vec::new()));
}

#[test]
fn push_over_an_existing_file_keeps_one_previous_copy() {
    let l = Loop::start();
    assert_eq!(l.push("a.txt", b"first").0, OK);
    assert_eq!(l.push("a.txt", b"second").0, OK);
    assert_eq!(fs::read(l.ex.join("a.txt")).unwrap(), b"second");
    assert_eq!(fs::read(l.ex.join(".ferry-prev/a.txt")).unwrap(), b"first");
    // The previous copies are not agent business: not listed, not readable.
    let (code, listing) = l.ls("");
    assert_eq!(code, OK);
    assert!(!listing.contains(".ferry-prev"), "{listing}");
    assert_eq!(l.pull(".ferry-prev/a.txt").0, BAD_PATH);
    assert_eq!(l.pull(".FERRY-PREV/a.txt").0, BAD_PATH);
}

#[test]
fn bad_requests_get_their_exit_code() {
    let l = Loop::start();
    assert_eq!(l.pull("missing.txt").0, BAD_PATH);
    assert_eq!(l.ls("missing").0, BAD_PATH);
    for x in ["../out.txt", "..\\out.txt", "/abs.txt", "C:/x.txt", "con.txt", "a?.txt", ".ferry-part-1-x", "trailing."] {
        assert_eq!(l.push(x, b"x").0, BAD_PATH, "{x}");
        assert_eq!(l.pull(x).0, BAD_PATH, "{x}");
    }
    assert_eq!(l.push("", b"x").0, BAD_PATH, "push needs a file name");
    let (r, _) = l.talk(&format!("push\tbig.bin\t{}", CAP + 1), None);
    assert_eq!(r.unwrap().0, TOO_LARGE);
    assert_eq!(l.talk("frobnicate\tx", None).0.unwrap().0, 1);
    assert!(leftovers(&l).is_empty());
    assert!(!l.root.join("out.txt").exists());
}

#[test]
fn no_serve_means_offline() {
    let dir = std::env::temp_dir().join(format!("ferry-test-{}-offline", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    assert!(remote::talk(&dir, "ls\t", None, &mut Vec::new()).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn end_of_channel_ends_serve() {
    let mut l = Loop::start();
    assert_eq!(l.push("a.txt", b"x").0, OK);
    assert_eq!(l.stop(), Some(0));
}

#[test]
fn matching_protocol_takes_the_lock_and_serves() {
    let l = Loop::start();
    let pid = fs::read_to_string(l.state.join("serve.lock")).unwrap();
    assert_eq!(pid, std::process::id().to_string());
    let mode = fs::metadata(l.state.join("ferry.sock")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(l.ls("").0, OK);
}

// A second Workstation with another protocol integer must not stop the
// running `serve`: the new `serve` exits before it takes the lock.
#[test]
fn channel_mismatch_leaves_the_running_serve_alone() {
    let l = Loop::start();
    let (mut ws, b) = UnixStream::pair().unwrap();
    let (st, b2) = (l.state.clone(), b.try_clone().unwrap());
    let serve = std::thread::spawn(move || remote::serve_on(&st, b, b2, false));
    ws.write_all(MAGIC).unwrap();
    ws.write_all(&frame(0, HELLO, b"1\t0.0.1\told-host")).unwrap();
    assert_eq!(serve.join().unwrap(), 1);
    assert_eq!(l.ls("").0, OK);
}

fn sock(l: &Loop) -> UnixStream {
    UnixStream::connect(l.state.join("ferry.sock")).unwrap()
}

#[test]
fn socket_mismatch_gets_end_1_and_a_hook_gets_a_log_line() {
    let l = Loop::start();
    let mut c = sock(&l);
    c.write_all(&remote::greet("1", "cli")).unwrap();
    let (_, kind, p) = read_frame(&mut c).unwrap();
    assert_eq!(kind, END);
    assert_eq!(fields(&p), ["1", &format!("ferry serve speaks protocol {PROTO}, this ferry 1")]);

    // A hook reads no reply: the connection just closes.
    let mut c = sock(&l);
    c.write_all(&remote::greet("1", "hook")).unwrap();
    assert!(read_frame(&mut c).is_err());
    assert!(fs::read_to_string(l.state.join("serve.log")).unwrap().contains("this ferry 1"));
    assert_eq!(l.ls("").0, OK);
}

#[test]
fn takeover_with_another_protocol_is_refused() {
    let l = Loop::start();
    let mut c = sock(&l);
    c.write_all(&remote::greet("1", "serve")).unwrap();
    c.write_all(&frame(0, TAKEOVER, b"other-host")).unwrap();
    let (_, kind, p) = read_frame(&mut c).unwrap();
    assert_eq!((kind, fields(&p)[0].as_str()), (END, "1"));
    assert_eq!(l.ls("").0, OK); // no BYE: the old `serve` stays
}

#[test]
fn takeover_replaces_the_old_serve() {
    let mut l = Loop::start();
    let sock = l.state.join("ferry.sock");
    let old = fs::metadata(&sock).unwrap().ino();
    let (ch2, serve2, ws2) = pair(&l.state, &l.ex, "new-host");

    // The old Workstation gets BYE with the host of the new one, then the old `serve` ends.
    let bye = l.ws.take().unwrap().join().unwrap().unwrap();
    assert!(bye.contains("new-host"), "{bye}");
    assert_eq!(l.serve.take().unwrap().join().unwrap(), 0);

    // The new `serve` binds a new socket file, then it serves.
    let t = Instant::now();
    while fs::metadata(&sock).map(|m| m.ino()).unwrap_or(old) == old {
        assert!(t.elapsed() < Duration::from_secs(5), "new serve did not bind");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(l.ls("").0, OK);
    ch2.shutdown(std::net::Shutdown::Both).unwrap();
    ws2.join().unwrap().unwrap();
    assert_eq!(serve2.join().unwrap(), 0);
}

#[test]
fn stale_socket_is_deleted_and_bound_again() {
    let l = Loop::start_with(|state| drop(UnixListener::bind(state.join("ferry.sock")).unwrap()));
    assert_eq!(l.ls("").0, OK);
}
