// Loopback tests: the Workstation core (`exchange::channel`) and `remote::serve_on`
// run in this process, joined by one socket pair, with a temporary Exchange
// folder and a temporary Remote state folder. The agent side is `remote::talk`
// on the helper socket of the state folder.

use crate::frame::*;
use crate::{exchange, remote};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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

impl Loop {
    fn start() -> Loop {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!("ferry-test-{}-{}", std::process::id(), N.fetch_add(1, SeqCst)));
        let (ex, state) = (root.join("ex"), root.join("state"));
        fs::create_dir_all(&ex).unwrap();
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&state).unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        let channel = a.try_clone().unwrap();
        let (st, ex2) = (state.clone(), ex.clone());
        let b2 = b.try_clone().unwrap();
        let serve = std::thread::spawn(move || remote::serve_on(&st, b, b2, false));
        let a2 = a.try_clone().unwrap();
        let ws = std::thread::spawn(move || exchange::channel(a2, a, &ex2, "test-host", || {}));
        let t = Instant::now();
        while !state.join("serve.sock").exists() {
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
