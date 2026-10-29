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
    let ws = std::thread::spawn(move || exchange::channel(a2, a, &ex, host, || {}, |_| {}));
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
            (fs::File::open(p).unwrap(), d.len() as u64)
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
    parts(&l.ex)
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

// ---- stream rules: a scripted peer on one side, the real code on the other ----

use std::io::Read;

fn temp_root(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let root = std::env::temp_dir().join(format!("ferry-test-{}-{tag}{}", std::process::id(), N.fetch_add(1, SeqCst)));
    fs::create_dir_all(&root).unwrap();
    root
}

fn wait_for(what: &str, ok: impl Fn() -> bool) {
    let t = Instant::now();
    while !ok() {
        assert!(t.elapsed() < Duration::from_secs(5), "timeout: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// The part files in an Exchange folder: all of `.ferry-part`, and each `.ferry-part*` entry elsewhere.
fn parts(ex: &Path) -> Vec<String> {
    let part = ex.join(".ferry-part");
    let (mut v, mut dirs) = (Vec::new(), vec![ex.to_path_buf()]);
    while let Some(d) = dirs.pop() {
        for e in fs::read_dir(&d).unwrap().map(Result::unwrap) {
            let p = e.path();
            if p != part && (d == part || e.file_name().to_string_lossy().starts_with(".ferry-part")) {
                v.push(p.display().to_string());
            }
            if p.is_dir() {
                dirs.push(p);
            }
        }
    }
    v
}

// A frame header with any length, and no payload.
fn header(len: usize, id: u32, kind: u8) -> Vec<u8> {
    let mut h = frame(id, kind, b"");
    h[..4].copy_from_slice(&(len as u32).to_be_bytes());
    h
}

fn end_reply(p: &[u8]) -> Reply {
    let f = fields(p);
    (f[0].parse().unwrap(), f[1].clone())
}

// A scripted Remote on the other end of the Workstation core.
struct FakeRemote {
    root: PathBuf,
    ex: PathBuf,
    s: UnixStream,
    ws: Option<JoinHandle<Result<String, String>>>,
    last_ls: u32,
}

impl FakeRemote {
    fn start() -> FakeRemote {
        let root = temp_root("remote");
        let ex = root.join("ex");
        fs::create_dir_all(&ex).unwrap();
        let (a, mut s) = UnixStream::pair().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (a2, ex2) = (a.try_clone().unwrap(), ex.clone());
        let ws = Some(std::thread::spawn(move || exchange::channel(a2, a, &ex2, "test-host", || {}, |_| {})));
        s.write_all(MAGIC).unwrap();
        s.write_all(&frame(0, HELLO, format!("{PROTO}\t0\tremote").as_bytes())).unwrap();
        let mut m = [0u8; 8];
        s.read_exact(&mut m).unwrap();
        assert_eq!(read_frame(&mut s).unwrap().1, HELLO);
        FakeRemote { root, ex, s, ws, last_ls: 1000 }
    }

    fn send(&mut self, id: u32, kind: u8, p: &[u8]) {
        self.s.write_all(&frame(id, kind, p)).unwrap();
    }

    fn next(&mut self) -> (u32, u8, Vec<u8>) {
        read_frame(&mut self.s).expect("a frame from the Workstation")
    }

    // The next frame must be the END of stream `id`.
    fn end_of(&mut self, id: u32) -> Reply {
        let (i, kind, p) = self.next();
        assert_eq!((i, kind), (id, END));
        end_reply(&p)
    }

    // No frame comes in reply to a dropped frame. The reply to an `ls` shows it: no other frame comes before.
    fn quiet(&mut self) {
        self.last_ls += 2;
        let id = self.last_ls;
        self.send(id, OPEN, b"ls\t");
        loop {
            let (i, kind, _) = self.next();
            assert_eq!(i, id, "a frame came for another stream");
            if kind == END {
                return;
            }
        }
    }

    // The channel ends when the Workstation core returns.
    fn ended(&mut self) -> Result<String, String> {
        self.ws.take().unwrap().join().unwrap()
    }

    fn part_has(&self, name: &str, data: &[u8]) -> bool {
        fs::read(self.ex.join(".ferry-part").join(name)).is_ok_and(|d| d == data)
    }
}

impl Drop for FakeRemote {
    fn drop(&mut self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
        if let Some(h) = self.ws.take() {
            let _ = h.join();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_push_completes_at_the_declared_size_with_no_end_from_the_sender() {
    let mut r = FakeRemote::start();
    r.send(2, OPEN, b"push\ta.txt\t5");
    r.send(2, DATA, b"he");
    wait_for("part file", || r.part_has("2-a.txt", b"he"));
    assert!(!r.ex.join("a.txt").exists());
    r.send(2, DATA, b"llo");
    assert_eq!(r.end_of(2).0, OK);
    assert_eq!(fs::read(r.ex.join("a.txt")).unwrap(), b"hello");
    assert!(parts(&r.ex).is_empty());

    // A declared size of 0 completes at OPEN, with no DATA.
    r.send(4, OPEN, b"push\tz.txt\t0");
    assert_eq!(r.end_of(4).0, OK);
    assert_eq!(fs::read(r.ex.join("z.txt")).unwrap(), b"");
}

#[test]
fn data_past_the_declared_size_gets_end_1_and_no_file() {
    let mut r = FakeRemote::start();
    r.send(2, OPEN, b"push\tbig.bin\t10");
    r.send(2, DATA, &[7; 20]);
    let (code, msg) = r.end_of(2);
    assert_eq!((code, msg.as_str()), (1, "more data than declared"));
    assert!(!r.ex.join("big.bin").exists());
    assert!(parts(&r.ex).is_empty());

    // The same when the border of the declared size sits inside the second frame.
    r.send(4, OPEN, b"push\tbig2.bin\t10");
    r.send(4, DATA, &[7; 6]);
    r.send(4, DATA, &[7; 6]);
    assert_eq!(r.end_of(4).0, 1);
    assert!(!r.ex.join("big2.bin").exists());
    assert!(parts(&r.ex).is_empty());

    // The stream is closed now: later frames get no reply.
    r.send(2, DATA, b"x");
    r.send(4, CANCEL, b"");
    r.quiet();
}

#[test]
fn the_workstation_drops_frames_for_closed_or_unknown_streams() {
    let mut r = FakeRemote::start();
    // Unknown streams.
    r.send(98, DATA, b"x");
    r.send(98, END, b"0\t");
    r.send(98, CANCEL, b"");
    // A push that completed: its stream is closed.
    r.send(2, OPEN, b"push\tdone.txt\t2");
    r.send(2, DATA, b"ab");
    assert_eq!(r.end_of(2).0, OK);
    r.send(2, DATA, b"cd");
    r.send(2, CANCEL, b"");
    // A push that the Workstation refused at OPEN: its data gets no second reply.
    r.send(4, OPEN, b"push\t../out.txt\t3");
    assert_eq!(r.end_of(4).0, BAD_PATH);
    r.send(4, DATA, b"abc");
    r.quiet();
    assert_eq!(fs::read(r.ex.join("done.txt")).unwrap(), b"ab");
    assert!(!r.root.join("out.txt").exists());
}

#[test]
fn cancel_closes_a_push_with_no_reply_and_deletes_the_part_file() {
    let mut r = FakeRemote::start();
    r.send(2, OPEN, b"push\tx.bin\t10");
    r.send(2, DATA, b"abc");
    wait_for("part file", || r.part_has("2-x.bin", b"abc"));
    r.send(2, CANCEL, b"");
    wait_for("part file deleted", || parts(&r.ex).is_empty());
    r.send(2, DATA, &[7; 7]); // the rest of the data is on the pipe already: dropped
    r.quiet();
    assert!(!r.ex.join("x.bin").exists());
}

#[test]
fn cancel_stops_the_answer_to_a_pull() {
    let mut r = FakeRemote::start();
    fs::write(r.ex.join("big.bin"), bytes(64 * CHUNK)).unwrap();
    r.send(2, OPEN, b"pull\tbig.bin");
    assert_eq!(r.next().1, DATA);
    r.send(2, CANCEL, b"");
    r.send(4, OPEN, b"ls\t");
    loop {
        let (id, kind, _) = r.next();
        assert!(!(id == 2 && kind == END), "END after CANCEL");
        if (id, kind) == (4, END) {
            break;
        }
    }
}

#[test]
fn a_frame_over_64_kib_ends_the_channel_and_deletes_part_files() {
    let mut r = FakeRemote::start();
    r.send(2, OPEN, b"push\tbig.bin\t100");
    r.send(2, DATA, &[7; 10]);
    wait_for("part file", || r.part_has("2-big.bin", &[7; 10]));
    r.s.write_all(&header(CHUNK + 1, 2, DATA)).unwrap();
    assert!(r.ended().is_ok());
    assert!(parts(&r.ex).is_empty());
}

#[test]
fn the_end_of_the_channel_deletes_part_files() {
    let mut r = FakeRemote::start();
    r.send(2, OPEN, b"push\tbig.bin\t100");
    r.send(2, DATA, &[7; 10]);
    wait_for("part file", || r.part_has("2-big.bin", &[7; 10]));
    r.s.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(r.ended().is_ok());
    assert!(parts(&r.ex).is_empty());
    assert!(!r.ex.join("big.bin").exists());
}

#[test]
fn the_start_empties_the_part_folder_and_keeps_the_rest() {
    let root = temp_root("start");
    let ex = root.join("ex");
    fs::create_dir_all(ex.join(".ferry-part/sub")).unwrap();
    fs::write(ex.join(".ferry-part/9-old.bin"), b"x").unwrap();
    fs::write(ex.join(".ferry-part/sub/old"), b"x").unwrap();
    fs::create_dir_all(ex.join(".ferry-prev")).unwrap();
    fs::write(ex.join(".ferry-prev/keep.txt"), b"x").unwrap();
    fs::write(ex.join("doc.txt"), b"x").unwrap();
    exchange::prepare(&ex).unwrap();
    assert!(ex.join(".ferry-part").is_dir());
    assert!(parts(&ex).is_empty());
    assert!(ex.join(".ferry-prev/keep.txt").exists() && ex.join("doc.txt").exists());
    // A first start makes the folders too.
    let fresh = root.join("fresh");
    exchange::prepare(&fresh).unwrap();
    assert!(fresh.join(".ferry-prev").is_dir() && fresh.join(".ferry-part").is_dir());
    fs::remove_dir_all(root).unwrap();
}

// A scripted Workstation on the other end of `serve_on`.
struct FakeWs {
    root: PathBuf,
    state: PathBuf,
    s: UnixStream,
    serve: Option<JoinHandle<i32>>,
}

impl FakeWs {
    fn start() -> FakeWs {
        let root = temp_root("ws");
        let state = root.join("state");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&state).unwrap();
        let (mut s, b) = UnixStream::pair().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (st, b2) = (state.clone(), b.try_clone().unwrap());
        let serve = Some(std::thread::spawn(move || remote::serve_on(&st, b, b2, false)));
        s.write_all(MAGIC).unwrap();
        s.write_all(&frame(0, HELLO, format!("{PROTO}\t0\tws").as_bytes())).unwrap();
        let mut m = [0u8; 8];
        s.read_exact(&mut m).unwrap();
        assert_eq!(read_frame(&mut s).unwrap().1, HELLO);
        let w = FakeWs { root, state, s, serve };
        wait_for("serve socket", || UnixStream::connect(w.state.join("ferry.sock")).is_ok());
        w
    }

    fn send(&mut self, id: u32, kind: u8, p: &[u8]) {
        self.s.write_all(&frame(id, kind, p)).unwrap();
    }

    fn next(&mut self) -> (u32, u8, Vec<u8>) {
        read_frame(&mut self.s).expect("a frame from serve")
    }

    // An agent connection that has said HELLO.
    fn agent(&self) -> UnixStream {
        let mut a = UnixStream::connect(self.state.join("ferry.sock")).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        a.write_all(&remote::greet(PROTO, "cli")).unwrap();
        a
    }
}

impl Drop for FakeWs {
    fn drop(&mut self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
        if let Some(h) = self.serve.take() {
            let _ = h.join();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

// Reads from `a` for a short time and says whether a frame came.
fn frame_comes(a: &mut UnixStream) -> bool {
    a.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let r = read_frame(a);
    a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    match r {
        Ok(_) => true,
        Err(e) => {
            assert!(matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut), "{e}");
            false
        }
    }
}

#[test]
fn serve_accepts_cancel_and_drops_frames_for_closed_or_unknown_streams() {
    let mut w = FakeWs::start();
    // Unknown streams, and a CANCEL for a stream that the Workstation could have opened.
    w.send(4, DATA, b"x");
    w.send(4, END, b"0\t");
    w.send(3, CANCEL, b"");
    w.send(4, CANCEL, b"");
    let mut a = w.agent();
    a.write_all(&frame(1, OPEN, b"ls\t")).unwrap();
    // `serve` sent no frame in reply to the dropped ones: the OPEN is the next frame.
    assert_eq!(w.next(), (2, OPEN, b"ls\t".to_vec()));
    w.send(2, DATA, b"row\n");
    w.send(2, END, b"0\t");
    assert_eq!(read_frame(&mut a).unwrap(), (2, DATA, b"row\n".to_vec()));
    assert_eq!(read_frame(&mut a).unwrap().1, END);

    // The stream is closed: frames for it go nowhere, and `serve` still works.
    w.send(2, DATA, b"late");
    w.send(2, END, b"1\tlate");
    w.send(2, CANCEL, b"");
    let mut b = w.agent();
    b.write_all(&frame(1, OPEN, b"ls\tb")).unwrap();
    assert_eq!(w.next(), (4, OPEN, b"ls\tb".to_vec()));
    assert!(!frame_comes(&mut a));
    assert!(!frame_comes(&mut b));
}

#[test]
fn serve_drops_agent_frames_for_another_stream() {
    let l = Loop::start();
    let mut a = sock(&l);
    a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    a.write_all(&remote::greet(PROTO, "cli")).unwrap();
    a.write_all(&frame(1, OPEN, b"push\tx.txt\t3")).unwrap();
    a.write_all(&frame(7, DATA, b"abc")).unwrap(); // not stream 1: dropped
    a.write_all(&frame(1, DATA, b"xyz")).unwrap();
    let (_, kind, p) = read_frame(&mut a).unwrap();
    assert_eq!((kind, end_reply(&p).0), (END, OK));
    assert_eq!(fs::read(l.ex.join("x.txt")).unwrap(), b"xyz");
}

#[test]
fn a_push_with_extra_data_through_serve_gets_end_1_and_no_file() {
    let l = Loop::start();
    let mut a = sock(&l);
    a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    a.write_all(&remote::greet(PROTO, "cli")).unwrap();
    a.write_all(&frame(1, OPEN, b"push\tbig.bin\t10")).unwrap();
    a.write_all(&frame(1, DATA, &[7; 20])).unwrap();
    let (_, kind, p) = read_frame(&mut a).unwrap();
    assert_eq!((kind, end_reply(&p)), (END, (1, "more data than declared".to_string())));
    assert!(!l.ex.join("big.bin").exists());
    assert!(leftovers(&l).is_empty());
}

#[test]
fn serve_ends_when_the_workstation_sends_a_frame_over_64_kib() {
    let mut w = FakeWs::start();
    let mut a = w.agent();
    a.write_all(&frame(1, OPEN, b"ls\t")).unwrap();
    assert_eq!(w.next().1, OPEN);
    w.s.write_all(&header(CHUNK + 1, 2, DATA)).unwrap();
    assert_eq!(w.serve.take().unwrap().join().unwrap(), 0);
    // The open request gets END 3.
    let (_, kind, p) = read_frame(&mut a).unwrap();
    assert_eq!((kind, end_reply(&p).0), (END, OFFLINE));
}

#[test]
fn serve_closes_an_agent_connection_that_sends_a_frame_over_64_kib() {
    let mut w = FakeWs::start();
    let mut a = w.agent();
    a.write_all(&frame(1, OPEN, b"push\tx\t100000")).unwrap();
    a.write_all(&header(CHUNK + 1, 1, DATA)).unwrap();
    assert_eq!(w.next(), (2, OPEN, b"push\tx\t100000".to_vec()));
    assert_eq!(w.next(), (2, CANCEL, Vec::new()));
    // Closed, not a timeout.
    assert!(!matches!(read_frame(&mut a).unwrap_err().kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));

    // The same for the first frame after the magic.
    let mut b = UnixStream::connect(w.state.join("ferry.sock")).unwrap();
    b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    b.write_all(MAGIC).unwrap();
    b.write_all(&header(CHUNK + 1, 0, HELLO)).unwrap();
    assert!(!matches!(read_frame(&mut b).unwrap_err().kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));

    // `serve` goes on.
    let mut c = w.agent();
    c.write_all(&frame(1, OPEN, b"ls\t")).unwrap();
    assert_eq!(w.next().1, OPEN);
}

// The Workstation core against a scripted Remote that sends `script` and then
// nothing. Returns the state of the link after the connection ended, the wait
// that `ended` gave, and whether the link was connected in between.
fn scripted(script: &[u8], stderr_line: &str) -> (crate::link::Chan, Option<u64>, bool) {
    let (a, mut b) = UnixStream::pair().unwrap();
    b.write_all(script).unwrap();
    b.shutdown(std::net::Shutdown::Write).unwrap();
    let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let up2 = up.clone();
    let link = crate::link::Link::new("alias", "key", || "09:12".into(), move |r| {
        up2.fetch_or(r.connected, SeqCst);
    });
    let a2 = a.try_clone().unwrap();
    let why = exchange::channel(a2, a, &std::env::temp_dir(), "ws", || {}, |f| link.hello(f)).unwrap();
    let wait = link.ended(&why, stderr_line);
    (link.chan(), wait, up.load(SeqCst))
}

fn remote_says(noise: usize, proto: &str, then: &[u8]) -> Vec<u8> {
    let mut v = vec![b' '; noise];
    v.extend(MAGIC);
    v.extend(frame(0, HELLO, format!("{proto}\t1.2.3\tbox").as_bytes()));
    v.extend(then);
    v
}

#[test]
fn login_output_up_to_4_kib_before_the_magic_is_skipped() {
    let bye = frame(0, BYE, b"replaced\tother-host");
    for noise in [0, 100, 4096] {
        let (chan, wait, _) = scripted(&remote_says(noise, PROTO, &bye), "");
        assert_eq!((chan, wait), (crate::link::Chan::Replaced { host: "other-host".into() }, None), "{noise} bytes");
    }
    let (chan, ..) = scripted(&remote_says(4097, PROTO, &bye), "");
    let crate::link::Chan::Offline { detail, .. } = chan else { panic!("{chan:?}") };
    assert!(detail.starts_with("Remote login prints to stdout"), "{detail}");
}

#[test]
fn a_matching_hello_connects_and_a_mismatched_one_does_not() {
    // The Remote then goes silent for good: the end is a plain channel end.
    let (chan, wait, up) = scripted(&remote_says(0, PROTO, b""), "");
    assert!(matches!(chan, crate::link::Chan::Offline { .. }) && wait == Some(5) && up, "{chan:?}");
    let (chan, wait, up) = scripted(&remote_says(0, "1", b""), "");
    assert_eq!((chan, wait, up), (crate::link::Chan::Mismatch { remote: "1".into(), wait: 5 }, Some(5), false));
}

#[test]
fn ssh_that_ends_before_the_hello_gives_its_last_stderr_line() {
    for script in [&b""[..], b"banner", MAGIC] {
        let (chan, ..) = scripted(script, "Permission denied (publickey).");
        let crate::link::Chan::Offline { detail, .. } = chan else { panic!("{chan:?}") };
        assert_eq!(detail, "ssh ended before HELLO: Permission denied (publickey).");
    }
}

// ---- agent exit codes and socket timeouts ----

const ENDED_TEXT: &str = "connection to the Workstation ended";

#[test]
fn a_socket_that_closes_with_no_end_gives_exit_3() {
    let dir = temp_root("eof");
    let l = UnixListener::bind(dir.join("ferry.sock")).unwrap();
    let h = std::thread::spawn(move || drop(l.accept().unwrap()));
    assert_eq!(remote::talk(&dir, "ls\t", None, &mut Vec::new()), Ok((OFFLINE, ENDED_TEXT.to_string())));
    h.join().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_channel_that_ends_gives_open_requests_exit_3() {
    for oversize in [false, true] {
        let mut w = FakeWs::start();
        let mut a = w.agent();
        a.write_all(&frame(1, OPEN, b"ls\t")).unwrap();
        assert_eq!(w.next().1, OPEN);
        if oversize {
            w.s.write_all(&header(CHUNK + 1, 2, DATA)).unwrap();
        } else {
            w.s.shutdown(std::net::Shutdown::Both).unwrap();
        }
        let (_, kind, p) = read_frame(&mut a).unwrap();
        assert_eq!((kind, end_reply(&p)), (END, (OFFLINE, ENDED_TEXT.to_string())), "oversize frame: {oversize}");
    }
}

#[test]
fn a_takeover_gives_open_requests_exit_3_replaced() {
    let w = FakeWs::start();
    let mut a = w.agent();
    a.write_all(&frame(1, OPEN, b"ls\t")).unwrap();
    let (mut ws2, b) = UnixStream::pair().unwrap();
    let (st, b2) = (w.state.clone(), b.try_clone().unwrap());
    let serve2 = std::thread::spawn(move || remote::serve_on(&st, b, b2, false));
    ws2.write_all(MAGIC).unwrap();
    ws2.write_all(&frame(0, HELLO, format!("{PROTO}\t0\tnew-host").as_bytes())).unwrap();
    let (_, kind, p) = read_frame(&mut a).unwrap();
    assert_eq!((kind, end_reply(&p)), (END, (OFFLINE, "replaced by a new connection".to_string())));
    // The new `serve` has written its HELLO before the channel closes.
    ws2.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut m = [0u8; 8];
    ws2.read_exact(&mut m).unwrap();
    assert_eq!(read_frame(&mut ws2).unwrap().1, HELLO);
    ws2.shutdown(std::net::Shutdown::Both).unwrap();
    assert_eq!(serve2.join().unwrap(), 0);
}

// A push with a local file that is shorter than the declared size, or that cannot be read:
// exit 1 at once, and `serve` cancels the stream.
#[test]
fn a_shorter_or_unreadable_local_file_gives_exit_1_and_a_cancel() {
    let mut w = FakeWs::start();
    let short = w.root.join("short");
    fs::write(&short, b"short").unwrap();
    let folder = w.root.join("folder");
    fs::create_dir(&folder).unwrap();
    for (file, declared) in [(short, 100), (folder, 10)] {
        let open = format!("push\tx\t{declared}");
        let r = remote::talk(&w.state, &open, Some((fs::File::open(file).unwrap(), declared)), &mut std::io::sink());
        assert_eq!(r.unwrap().0, 1);
        let id = loop {
            let (id, kind, _) = w.next();
            if kind == CANCEL {
                break id;
            }
        };
        assert_eq!(id % 2, 0, "a stream of the Remote");
    }
}

// An agent that does not read: `serve` drops it after 1 s, sends CANCEL, and the channel goes on.
#[test]
fn an_agent_that_does_not_read_is_dropped_with_a_cancel() {
    let mut w = FakeWs::start();
    let mut a = w.agent();
    a.write_all(&frame(1, OPEN, b"pull\tbig")).unwrap();
    assert_eq!(w.next(), (2, OPEN, b"pull\tbig".to_vec()));
    // More than the socket buffers hold. The writes end when `serve` has dropped the agent.
    for _ in 0..24 {
        w.send(2, DATA, &[7; CHUNK]);
    }
    assert_eq!(w.next(), (2, CANCEL, Vec::new()));
    // The agent gets what the buffers held, then a closed socket and no END.
    loop {
        match read_frame(&mut a) {
            Ok((_, kind, _)) => assert_eq!(kind, DATA),
            Err(e) => {
                assert!(!matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut), "{e}");
                break;
            }
        }
    }
    let mut b = w.agent();
    b.write_all(&frame(1, OPEN, b"ls\t")).unwrap();
    assert_eq!(w.next(), (4, OPEN, b"ls\t".to_vec()));
}

// A request that cannot fit in one OPEN frame is a bad command line: exit 2 before any socket opens.
#[test]
fn an_xpath_over_one_frame_gives_exit_2() {
    let long = "a".repeat(CHUNK);
    assert_eq!(remote::ls(&long), 2);
    assert_eq!(remote::pull(&long, None), 2);
    assert_eq!(remote::push(&std::env::current_exe().unwrap().to_string_lossy(), &long), 2);
}

#[test]
fn ferry_names_are_refused_in_any_case() {
    let l = Loop::start();
    assert_eq!(l.push("a.txt", b"1").0, OK);
    assert_eq!(l.push("a.txt", b"2").0, OK); // .ferry-prev/a.txt exists now
    for x in [".FERRY-PART/x", ".Ferry-Prev/a.txt", ".ferry-prev\\a.txt", "sub/.FERRY-x", ".Ferry-x"] {
        assert_eq!(l.push(x, b"x").0, BAD_PATH, "push {x}");
        assert_eq!(l.pull(x).0, BAD_PATH, "pull {x}");
        assert_eq!(l.ls(x).0, BAD_PATH, "ls {x}");
    }
    assert!(!l.ex.join("sub").exists());
    assert!(leftovers(&l).is_empty());
}

#[test]
fn refused_file_types_are_refused_in_each_folder() {
    let l = Loop::start();
    for x in ["a.url", "A.LNK", "x.Library-MS", "s.searchConnector-ms", "q.scf", "desktop.ini", "Desktop.INI", "sub/deep/b.URL", "sub/DESKTOP.ini"] {
        let (code, msg) = l.push(x, b"x");
        let name = x.rsplit('/').next().unwrap();
        assert_eq!((code, msg), (BAD_PATH, format!("refused file type: {name}")), "{x}");
        assert!(!l.ex.join(x).exists(), "{x}");
    }
    assert!(!l.ex.join("sub").exists());
    for x in ["a.urlx", "url", "x.lnk.txt", "desktop.ini.txt", "my-desktop.ini"] {
        assert_eq!(l.push(x, b"x").0, OK, "{x}");
    }
}

#[test]
fn folders_and_files_in_the_wrong_place() {
    let l = Loop::start();
    assert_eq!(l.push("d/in.txt", b"x").0, OK);
    assert_eq!(l.push("f.txt", b"x").0, OK);

    assert_eq!(l.push("d", b"x"), (BAD_PATH, "is a folder: d".to_string()));
    assert_eq!(l.pull("d").0, BAD_PATH);
    assert_eq!(l.talk("pull\td", None).0.unwrap().1, "is a folder: d");
    assert_eq!(l.talk("ls\tf.txt", None).0.unwrap(), (BAD_PATH, "not a folder: f.txt".to_string()));
    assert_eq!(l.push("f.txt/x.txt", b"x").0, BAD_PATH);
    assert_eq!(l.push("f.txt/deeper/x.txt", b"x").0, BAD_PATH);

    // The folder stays where it is, and no previous copy of it exists.
    assert_eq!(fs::read(l.ex.join("d/in.txt")).unwrap(), b"x");
    assert_eq!(fs::read(l.ex.join("f.txt")).unwrap(), b"x");
    assert!(!l.ex.join(".ferry-prev/d").exists() && !l.ex.join(".ferry-prev/f.txt").exists());
    assert!(leftovers(&l).is_empty());
}

#[test]
fn ls_hides_ferry_names_and_office_owner_files() {
    let l = Loop::start();
    for p in [".FERRY-x", ".Ferry-part2", "~$book.xlsx", "sub/~$doc.docx", "sub/.ferry-y", "~a.txt", "sub/keep.txt", "a.txt"] {
        let p = l.ex.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }
    let names = |d: &str| -> Vec<String> {
        l.ls(d).1.lines().map(|r| r.split('\t').nth(2).unwrap().to_string()).collect()
    };
    assert_eq!(names(""), ["a.txt", "sub/", "~a.txt"]);
    assert_eq!(names("sub"), ["sub/keep.txt"]);
}

#[test]
fn a_pull_over_the_cap_gets_exit_5() {
    let l = Loop::start();
    let f = fs::File::create(l.ex.join("big.bin")).unwrap();
    f.set_len(CAP + 1).unwrap(); // sparse
    assert_eq!(l.pull("big.bin"), (TOO_LARGE, Vec::new()));
}

#[test]
fn a_push_needs_2_gib_free_after_the_write() {
    const GIB: u64 = 1 << 30;
    let l = Loop::start();
    let free = |n: u64| exchange::FREE.lock().unwrap().insert(l.ex.clone(), n);
    free(2 * GIB + 4);
    assert_eq!(l.push("ok.txt", b"1234").0, OK, "exactly 2 GiB stay free");
    free(2 * GIB + 3);
    let (code, msg) = l.push("low.txt", b"1234");
    assert_eq!((code, msg), (TOO_LARGE, "Workstation disk low: 2 GiB free".to_string()));
    assert!(!l.ex.join("low.txt").exists());
    free(GIB + GIB / 2);
    assert_eq!(l.push("low.txt", b"").1, "Workstation disk low: 1 GiB free");
    assert!(leftovers(&l).is_empty());
    free(u64::MAX);
    assert_eq!(l.push("low.txt", b"1").0, OK);
    exchange::FREE.lock().unwrap().remove(&l.ex);
}

// A sharing violation does not exist on Linux. A folder that refuses the move of
// its entry gives the same error kind (access denied), which Windows also maps to exit 4.
#[test]
fn a_file_in_use_gives_exit_4_and_stays_as_it_was() {
    let l = Loop::start();
    assert_eq!(l.push("sub/a.txt", b"old").0, OK);
    let sub = l.ex.join("sub");
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();
    // A user with the right to write anyway (root) cannot make the lock.
    let can_write = fs::write(sub.join("probe"), b"").is_ok();
    let r = l.push("sub/a.txt", b"new");
    let (pull_code, _) = l.pull("sub/a.txt");
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
    if can_write {
        return;
    }
    assert_eq!(r, (IN_USE, "in use: sub/a.txt".to_string()));
    assert_eq!(pull_code, OK);
    assert_eq!(fs::read(sub.join("a.txt")).unwrap(), b"old");
    assert!(leftovers(&l).is_empty());

    // A file that the Workstation cannot open is in use for a pull.
    fs::set_permissions(sub.join("a.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(l.talk("pull\tsub/a.txt", None).0.unwrap(), (IN_USE, "in use: sub/a.txt".to_string()));
}

#[test]
fn a_push_without_a_valid_size_changes_nothing() {
    let l = Loop::start();
    for open in ["push\ta.txt", "push\ta.txt\tabc", "push\ta.txt\t-1", "push", "pull"] {
        let code = l.talk(open, None).0.unwrap().0;
        assert_ne!(code, OK, "{open}");
    }
    assert!(!l.ex.join("a.txt").exists());
    assert!(leftovers(&l).is_empty());
}
