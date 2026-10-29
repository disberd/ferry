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
    let ws = std::thread::spawn(move || exchange::channel(a2, a, &ex, host, || {}, |_| {}, |_| false));
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
        let ws = Some(std::thread::spawn(move || exchange::channel(a2, a, &ex2, "test-host", || {}, |_| {}, |_| false)));
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
        FakeWs::start_with("no-herdr.sock")
    }

    // `herdr` is the file name of the herdr socket in the root folder. Nothing listens on it
    // until the test binds it. The first `HERDR` frame is read here, so the tests that follow
    // see the frames of their own requests.
    fn start_with(herdr: &str) -> FakeWs {
        let mut w = FakeWs::launch(herdr, Duration::from_secs(60), |_| {});
        assert_eq!(w.next().1, HERDR);
        w
    }

    // `serve` with a herdr check every `every`. The test reads the `HERDR` frames. `prep` gets
    // the herdr socket path before `serve` starts, so a fake herdr can listen for the first check.
    fn launch(herdr: &str, every: Duration, prep: impl FnOnce(&Path)) -> FakeWs {
        let root = temp_root("ws");
        let state = root.join("state");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&state).unwrap();
        let (mut s, b) = UnixStream::pair().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (st, b2, herdr) = (state.clone(), b.try_clone().unwrap(), root.join(herdr));
        prep(&herdr);
        let serve = Some(std::thread::spawn(move || remote::serve_with(&st, herdr, every, b, b2, false)));
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
    let why = exchange::channel(a2, a, &std::env::temp_dir(), "ws", || {}, |f| link.hello(f), |f| link.herdr(f)).unwrap();
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

// ---- hotkey press ----

use crate::exchange::{Item, Outcome, Src};
use std::sync::{Arc, Mutex};

// The Landing folder entries under a state folder.
fn entries(state: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(state.join("in")).map(|d| d.map(|e| e.unwrap().path()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

// `YYYYmmdd-HHMMSS`, with `-2`, `-3` and so on when the name existed.
fn is_stamp(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 15 && b[8] == b'-' && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit) && (b.len() == 15 || b[15] == b'-')
}

// A press on a full loop. The second value is the text that the press put on the clipboard.
fn pressed(l: &Loop, items: &[Item]) -> (Outcome, Option<String>) {
    wait_for("channel", || exchange::connected(&l.ex));
    let mut clip = None;
    let out = exchange::press(&l.ex, items, Duration::ZERO, || {}, |t| {
        clip = Some(t.to_string());
        true
    });
    (out, clip)
}

fn clip_paths(out: Outcome) -> Vec<String> {
    let Outcome::Clip(paths) = out else { panic!("{out:?}") };
    paths
}

#[test]
fn a_press_lands_its_files_in_one_entry_and_gives_the_paths_for_the_clipboard() {
    let l = Loop::start();
    let big = bytes(3 * CHUNK + 5);
    let (f, e) = (l.root.join("big.bin"), l.root.join("empty.txt"));
    fs::write(&f, &big).unwrap();
    fs::write(&e, b"").unwrap();
    let items = [Item::file(&f).unwrap(), Item::mem("shot.png", b"png".to_vec()), Item::file(&e).unwrap()];
    let (out, clip) = pressed(&l, &items);
    let paths = clip_paths(out);
    assert_eq!(clip.as_deref(), Some(paths.join(" ").as_str()));
    let dirs = entries(&l.state);
    assert_eq!(dirs.len(), 1);
    assert!(is_stamp(dirs[0].file_name().unwrap().to_str().unwrap()), "{dirs:?}");
    assert_eq!(fs::metadata(&dirs[0]).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(paths, ["big.bin", "shot.png", "empty.txt"].map(|n| dirs[0].join(n).display().to_string()));
    assert_eq!(fs::read(&paths[0]).unwrap(), big);
    assert_eq!(fs::read(&paths[1]).unwrap(), b"png");
    assert_eq!(fs::read(&paths[2]).unwrap(), b"");
    assert_eq!(names(&dirs[0]).len(), 3, "a part file is left");
}

#[test]
fn each_press_makes_its_own_entry() {
    let l = Loop::start();
    for _ in 0..2 {
        clip_paths(pressed(&l, &[Item::mem("a.txt", b"1".to_vec())]).0);
    }
    let dirs = entries(&l.state);
    assert_eq!(dirs.len(), 2);
    assert!(dirs.iter().all(|d| is_stamp(d.file_name().unwrap().to_str().unwrap()) && names(d) == ["a.txt"]), "{dirs:?}");
}

#[test]
fn names_keep_safe_characters_and_a_repeat_gets_a_number_before_the_extension() {
    let l = Loop::start();
    let items = ["a b.txt", "a?b.txt", ".hid", ".hid", "x.tar.gz", "x.tar.gz", "..", "é.txt", "ü.txt"].map(|n| Item::mem(n, b"1".to_vec()));
    let paths = clip_paths(pressed(&l, &items).0);
    let got: Vec<&str> = paths.iter().map(|p| p.rsplit('/').next().unwrap()).collect();
    assert_eq!(got, ["a_b.txt", "a_b-2.txt", ".hid", ".hid-2", "x.tar.gz", "x.tar-2.gz", "_", "_.txt", "_-2.txt"]);
}

#[test]
fn a_new_entry_deletes_entries_older_than_7_days() {
    let l = Loop::start_with(|state| {
        for (name, days) in [("old", 8), ("recent", 6)] {
            let d = state.join("in").join(name);
            fs::create_dir_all(&d).unwrap();
            fs::File::open(&d).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(days * 86400)).unwrap();
        }
    });
    clip_paths(pressed(&l, &[Item::mem("a.txt", b"1".to_vec())]).0);
    let left: Vec<String> = entries(&l.state).iter().map(|d| d.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert!(left.len() == 2 && left.contains(&"recent".to_string()) && !left.contains(&"old".to_string()), "{left:?}");
}

#[test]
fn a_pull_with_no_local_path_lands_in_a_new_entry_with_a_safe_name() {
    let l = Loop::start();
    fs::write(l.ex.join("my file (1).txt"), b"data").unwrap();
    assert_eq!(remote::pull_in(&l.state, "my file (1).txt", None), 0);
    assert_eq!(remote::pull_in(&l.state, "my file (1).txt", None), 0);
    let dirs = entries(&l.state);
    assert_eq!(dirs.len(), 2);
    for d in &dirs {
        assert!(is_stamp(d.file_name().unwrap().to_str().unwrap()), "{d:?}");
        assert_eq!(names(d), ["my_file__1_.txt"]);
        assert_eq!(fs::read(d.join("my_file__1_.txt")).unwrap(), b"data");
    }
}

#[test]
fn a_press_of_100_files_with_the_longest_names_fits_in_one_open_and_one_reply() {
    let l = Loop::start();
    let items: Vec<Item> = (0..100).map(|i| Item::mem(&format!("{i:03}{}.txt", "n".repeat(248)), vec![i as u8])).collect();
    let paths = clip_paths(pressed(&l, &items).0);
    assert_eq!(paths.len(), 100);
    // The Remote cuts a name to 240 bytes and keeps its extension.
    assert!(paths.iter().all(|p| p.rsplit('/').next().unwrap().len() == 240 && p.ends_with(".txt")));
    assert!(paths.iter().enumerate().all(|(i, p)| fs::read(p).unwrap() == [i as u8]));
}

#[test]
fn a_press_over_the_limits_is_refused_and_sends_nothing() {
    let mut r = FakeRemote::start();
    wait_for("channel", || exchange::connected(&r.ex));
    let many: Vec<Item> = (0..101).map(|i| Item::mem(&format!("f{i}"), vec![])).collect();
    let long: Vec<Item> = (0..100).map(|i| Item::mem(&format!("{i}{}", "n".repeat(700)), vec![])).collect();
    for items in [many, long] {
        let out = exchange::press(&r.ex, &items, Duration::ZERO, || {}, |_| panic!("clipboard"));
        assert!(matches!(&out, Outcome::Failed(w) if w.starts_with("press refused")), "{out:?}");
    }
    r.quiet();
}

#[test]
fn a_press_with_no_channel_fails() {
    let out = exchange::press(Path::new("/no-such-exchange-folder"), &[Item::mem("a", vec![1])], Duration::ZERO, || {}, |_| panic!("clipboard"));
    assert!(matches!(out, Outcome::Failed(_)));
}

#[test]
fn a_press_over_the_cap_gets_end_5_and_nothing_lands() {
    let l = Loop::start();
    let big = l.root.join("big.bin");
    fs::File::create(&big).unwrap().set_len(CAP + 1).unwrap(); // sparse
    let (out, clip) = pressed(&l, &[Item::file(&big).unwrap()]);
    assert!(matches!(&out, Outcome::Failed(w) if w.contains("END 5")), "{out:?}");
    assert!(clip.is_none() && entries(&l.state).is_empty());
}

// A scripted Workstation opens presses on `serve`.
fn ws_open(w: &mut FakeWs, id: u32, files: &[(&str, u64)]) {
    let mut open = format!("hotkey\t{}", files.len());
    for (n, s) in files {
        open += &format!("\t{n}\t{s}");
    }
    w.send(id, OPEN, open.as_bytes());
}

// The next frame from `serve` must be the END of stream `id`.
fn ws_end(w: &mut FakeWs, id: u32) -> Reply {
    let (i, kind, p) = w.next();
    assert_eq!((i, kind), (id, END));
    end_reply(&p)
}

#[test]
fn serve_splits_the_bytes_by_the_declared_sizes_and_renames_after_the_last_byte() {
    let mut w = FakeWs::start();
    ws_open(&mut w, 1, &[("a", 2), ("z1", 0), ("b", 3), ("z2", 0)]);
    wait_for("entry", || entries(&w.state).len() == 1);
    let dir = entries(&w.state).remove(0);
    w.send(1, DATA, b"x");
    wait_for("part file", || fs::read(dir.join("a.ferry-part")).is_ok_and(|d| d == b"x"));
    assert_eq!(names(&dir), ["a.ferry-part"]);
    // One frame holds the end of `a`, all of `b`, and the empty files between and after them.
    w.send(1, DATA, b"yBCD");
    let (code, text) = ws_end(&mut w, 1);
    assert_eq!(code, OK);
    let paths: Vec<&str> = text.strip_prefix("clip ").unwrap().split(' ').collect();
    assert_eq!(paths, ["a", "z1", "b", "z2"].map(|n| dir.join(n).display().to_string()));
    assert_eq!(names(&dir), ["a", "b", "z1", "z2"]);
    let read = |i: usize| fs::read(paths[i]).unwrap();
    assert_eq!((read(0), read(1), read(2)), (b"xy".to_vec(), vec![], b"BCD".to_vec()));

    // A press of empty files completes at OPEN.
    ws_open(&mut w, 3, &[("empty", 0)]);
    assert_eq!(ws_end(&mut w, 3).0, OK);
    assert_eq!(entries(&w.state).len(), 2);
}

#[test]
fn serve_refuses_a_bad_or_over_cap_press_at_open_and_drops_its_data() {
    let mut w = FakeWs::start();
    for (id, open) in [(1, "hotkey\t2\ta\t1"), (3, "hotkey\t0"), (5, "hotkey\t1\ta\tx"), (7, "other\t1\ta\t1")] {
        w.send(id, OPEN, open.as_bytes());
        assert_eq!(ws_end(&mut w, id).0, 1, "{open}");
    }
    ws_open(&mut w, 9, &[("a", CAP / 2 + 1), ("b", CAP / 2)]);
    assert_eq!(ws_end(&mut w, 9).0, TOO_LARGE);
    ws_open(&mut w, 11, &[("a", u64::MAX), ("b", 1)]);
    assert_eq!(ws_end(&mut w, 11).0, TOO_LARGE);
    w.send(9, DATA, b"late");
    w.send(11, CANCEL, b"");
    assert!(entries(&w.state).is_empty());

    // A sum of exactly the cap is fine. A CANCEL then deletes the entry, and `serve` sends no frame.
    ws_open(&mut w, 13, &[("big", CAP)]);
    wait_for("entry", || entries(&w.state).len() == 1);
    w.send(13, CANCEL, b"");
    wait_for("entry deleted", || entries(&w.state).is_empty());
    ws_open(&mut w, 15, &[("c", 0)]);
    assert_eq!(ws_end(&mut w, 15).0, OK);
}

#[test]
fn serve_ends_a_press_that_gets_more_data_than_declared_and_deletes_the_entry() {
    let mut w = FakeWs::start();
    ws_open(&mut w, 1, &[("a", 3)]);
    w.send(1, DATA, b"abcd");
    assert_eq!(ws_end(&mut w, 1).0, 1);
    assert!(entries(&w.state).is_empty());
    w.send(1, DATA, b"x"); // the stream is closed: no reply
    ws_open(&mut w, 3, &[("c", 0)]);
    assert_eq!(ws_end(&mut w, 3).0, OK);
}

#[test]
fn the_end_of_the_channel_deletes_the_entry_of_a_press_in_progress() {
    let mut w = FakeWs::start();
    ws_open(&mut w, 1, &[("a", 10)]);
    w.send(1, DATA, b"abc");
    wait_for("part file", || entries(&w.state).first().is_some_and(|d| fs::read(d.join("a.ferry-part")).is_ok_and(|b| b == b"abc")));
    w.s.shutdown(std::net::Shutdown::Both).unwrap();
    w.serve.take().unwrap().join().unwrap();
    assert!(entries(&w.state).is_empty());
}

// One press of one small file against a scripted Remote that answers with END `code` and `text`.
// The second value is the text that the press put on the clipboard.
fn answered(code: u8, text: &str) -> (Outcome, Option<String>) {
    let mut r = FakeRemote::start();
    let ex = r.ex.clone();
    let clip = Arc::new(Mutex::new(None));
    let clip2 = clip.clone();
    let h = std::thread::spawn(move || {
        wait_for("channel", || exchange::connected(&ex));
        exchange::press(&ex, &[Item::mem("a.png", b"abc".to_vec())], Duration::ZERO, || {}, move |t| {
            *clip2.lock().unwrap() = Some(t.to_string());
            true
        })
    });
    let (id, kind, p) = r.next();
    assert_eq!((id, kind, p.as_slice()), (1, OPEN, &b"hotkey\t1\ta.png\t3"[..]));
    assert_eq!(r.next(), (1, DATA, b"abc".to_vec()));
    r.send(1, END, &end(code, text));
    let out = h.join().unwrap();
    let clip = clip.lock().unwrap().clone();
    (out, clip)
}

#[test]
fn a_clip_reply_reaches_the_clipboard_only_when_each_path_passes_the_check() {
    let good = "/home/u/.local/state/ferry/in/20261029-101010/a.png /x/b-2.txt";
    let (out, clip) = answered(OK, &format!("clip {good}"));
    assert_eq!(out, Outcome::Clip(good.split(' ').map(String::from).collect()));
    assert_eq!(clip.as_deref(), Some(good));
    for bad in [
        "clip ", "clip relative/a", "clip /", "clip /a b", "clip /a;rm", "clip /a$(id)", "clip /a`id`", "clip /a'b", "clip /a\"b",
        "clip /a /c|d", "clip /a\\b", "clip /é", "clip  /a", "clip /a /b ", "clip", "other /a",
    ] {
        let (out, clip) = answered(OK, bad);
        assert!(matches!(out, Outcome::Failed(_)), "{bad:?}: {out:?}");
        assert!(clip.is_none(), "{bad:?}");
    }
}

#[test]
fn an_end_with_a_code_fails_the_press_and_a_pasted_reply_leaves_the_clipboard() {
    for (code, text) in [(TOO_LARGE, "too large"), (1, "x"), (3, "gone")] {
        let (out, clip) = answered(code, text);
        assert!(matches!(out, Outcome::Failed(_)) && clip.is_none(), "{code}: {out:?}");
    }
    assert_eq!(answered(OK, "pasted"), (Outcome::Pasted, None));
}

#[test]
fn a_file_that_is_shorter_or_unreadable_gives_cancel_and_the_error() {
    let mut r = FakeRemote::start();
    wait_for("channel", || exchange::connected(&r.ex));
    let short = r.root.join("short");
    fs::write(&short, b"abc").unwrap();
    for (n, (path, size)) in [(short, 10), (r.root.join("gone"), 5)].into_iter().enumerate() {
        let id = 1 + 2 * n as u32;
        let item = Item { name: "f.bin".into(), size, src: Src::File(path) };
        let out = exchange::press(&r.ex, &[item], Duration::ZERO, || {}, |_| panic!("clipboard"));
        assert!(matches!(out, Outcome::Failed(_)), "{out:?}");
        // OPEN, then maybe DATA, then CANCEL, and no END.
        let (i, kind, _) = r.next();
        assert_eq!((i, kind), (id, OPEN));
        loop {
            let (i, kind, _) = r.next();
            assert_eq!(i, id);
            match kind {
                DATA => {}
                CANCEL => break,
                k => panic!("kind {k}"),
            }
        }
    }
    r.quiet();
}

#[test]
fn serve_deletes_the_entry_of_a_press_that_the_workstation_cancels() {
    let l = Loop::start();
    let short = l.root.join("short");
    fs::write(&short, b"abc").unwrap();
    let item = Item { name: "f.bin".into(), size: 10, src: Src::File(short) };
    let (out, clip) = pressed(&l, &[item]);
    assert!(matches!(out, Outcome::Failed(_)) && clip.is_none(), "{out:?}");
    // The press ended at its CANCEL. `serve` handles OPEN, DATA and CANCEL in order, so a press
    // that completes after the CANCEL finds the entry gone.
    clip_paths(pressed(&l, &[Item::mem("ok.txt", b"1".to_vec())]).0);
    let dirs = entries(&l.state);
    assert!(dirs.len() == 1 && names(&dirs[0]) == ["ok.txt"], "{dirs:?}");
}

#[test]
fn the_workstation_stops_sending_data_after_an_end() {
    let mut r = FakeRemote::start();
    let big = r.root.join("big.bin");
    fs::File::create(&big).unwrap().set_len(CAP + 1).unwrap(); // sparse
    let ex = r.ex.clone();
    let h = std::thread::spawn(move || {
        wait_for("channel", || exchange::connected(&ex));
        exchange::press(&ex, &[Item::file(&big).unwrap()], Duration::ZERO, || {}, |_| panic!("clipboard"))
    });
    let (id, kind, _) = r.next();
    assert_eq!(kind, OPEN);
    r.send(id, END, &end(TOO_LARGE, "too large"));
    // Count the frames that were on the way. The file holds 8192 chunks.
    r.s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let mut n = 0;
    while read_frame(&mut r.s).is_ok() {
        n += 1;
    }
    r.s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    assert!(n < 64, "{n} frames after the END");
    assert!(matches!(h.join().unwrap(), Outcome::Failed(_)));
    r.quiet();
}

fn asked(s: crate::sounds::Sound) -> usize {
    crate::sounds::ASKED.lock().unwrap().iter().filter(|&&x| x == s).count()
}

#[test]
fn a_hook_alert_reaches_the_workstation_as_its_sound() {
    use crate::sounds::Sound::{Blocked, Done};
    let l = Loop::start();
    let event = |status: &str| format!(r#"{{"event":"pane.agent_status_changed","data":{{"pane_id":"w1:p2","workspace_id":"w1","agent_status":"{status}","agent":"claude"}}}}"#);
    // Only these two tests ask for Blocked and Done, so the counts are exact.
    for status in ["working", "idle", "unknown"] {
        assert_eq!(remote::hook(&l.state, &event(status)), 0);
    }
    assert_eq!(remote::hook(&l.state, &event("blocked")), 0);
    wait_for("the Blocked sound", || asked(Blocked) == 1);
    assert_eq!(remote::hook(&l.state, &event("done")), 0);
    wait_for("the Done sound", || asked(Done) == 1);
    assert_eq!(asked(Blocked), 1);
}

#[test]
fn a_hook_sends_one_alert_for_blocked_and_done_only() {
    let root = temp_root("hook");
    let sock = UnixListener::bind(root.join("ferry.sock")).unwrap();
    sock.set_nonblocking(true).unwrap();
    let event = |status: &str| format!(r#"{{"data": {{"agent_status" : "{status}", "pane_id": "w1:p1"}}}}"#);
    assert_eq!(remote::hook(&root, &event("working")), 0);
    assert_eq!(remote::hook(&root, "not json"), 0);
    assert!(sock.accept().is_err(), "a hook connected for a status that needs no alert");

    assert_eq!(remote::hook(&root, &event("done")), 0);
    let (mut c, _) = sock.accept().unwrap();
    c.set_nonblocking(false).unwrap();
    let mut got = Vec::new();
    std::io::Read::read_to_end(&mut c, &mut got).unwrap();
    assert_eq!(got, [remote::greet(PROTO, "hook"), frame(0, ALERT, b"done\tw1:p1")].concat());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_hook_with_no_serve_exits_quietly() {
    let root = temp_root("hook-off");
    // No socket file: ENOENT.
    assert_eq!(remote::hook(&root, r#"{"agent_status":"blocked","pane_id":"w1:p1"}"#), 0);
    // A socket file that nobody listens on: ECONNREFUSED.
    drop(UnixListener::bind(root.join("ferry.sock")).unwrap());
    assert_eq!(remote::hook(&root, r#"{"agent_status":"done","pane_id":"w1:p1"}"#), 0);
    assert!(!root.join("serve.log").exists());
    let _ = fs::remove_dir_all(root);
}

// How a fake herdr answers one connection.
enum Herdr {
    Say(&'static str),
    Close,
    Hang,
}

const PANE: &str = r#"{"id":"1","result":{"type":"pane_info","pane":{"pane_id":"w1:p2","focused":true}}}"#;
const SENT: &str = r#"{"id":"2","result":{"type":"ok"}}"#;

// A herdr on a Unix socket. Connection n reads one line, records it, and acts as `script[n]`.
// The returned list holds the request lines in order.
fn fake_herdr(sock: &Path, script: Vec<Herdr>) -> Arc<Mutex<Vec<String>>> {
    use std::io::{BufRead, BufReader, Read};
    let listener = UnixListener::bind(sock).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for act in script {
            let (mut c, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(c.try_clone().unwrap()).read_line(&mut line).unwrap();
            seen2.lock().unwrap().push(line.trim_end().to_string());
            match act {
                Herdr::Say(r) => writeln!(c, "{r}").unwrap(),
                Herdr::Close => {}
                // Wait for the caller to give up and close.
                Herdr::Hang => while c.read(&mut [0u8; 1]).is_ok_and(|n| n > 0) {},
            }
        }
    });
    seen
}

// One press of two files against `serve`. `script` is the fake herdr, or None for a socket that
// does not exist. Returns the END reply, the request lines that herdr got, and the two paths.
fn herdr_press(script: Option<Vec<Herdr>>) -> (Reply, Vec<String>, String) {
    let mut w = FakeWs::start_with("herdr.sock");
    let seen = script.map(|s| fake_herdr(&w.root.join("herdr.sock"), s)).unwrap_or_default();
    ws_open(&mut w, 1, &[("a.png", 1), ("b.png", 0)]);
    w.send(1, DATA, b"x");
    let reply = ws_end(&mut w, 1);
    let dir = entries(&w.state).remove(0);
    let paths = format!("{} {}", dir.join("a.png").display(), dir.join("b.png").display());
    let seen = seen.lock().unwrap().clone();
    (reply, seen, paths)
}

#[test]
fn a_press_types_the_paths_into_the_focused_pane_and_replies_pasted() {
    let (reply, seen, paths) = herdr_press(Some(vec![Herdr::Say(PANE), Herdr::Say(SENT)]));
    assert_eq!(reply, (OK, "pasted".into()));
    assert_eq!(seen.len(), 2);
    // The first call names no calling pane, so herdr gives the focused pane.
    assert!(seen[0].contains(r#""method":"pane.current""#) && seen[0].contains(r#""params":{}"#), "{}", seen[0]);
    assert!(!seen[0].contains("caller_pane_id"));
    assert!(seen[1].contains(r#""method":"pane.send_input""#), "{}", seen[1]);
    assert!(seen[1].contains(&format!(r#""params":{{"pane_id":"w1:p2","text":"{paths}"}}"#)), "{}", seen[1]);
}

#[test]
fn a_press_falls_back_to_the_clipboard_when_the_paste_is_not_possible() {
    let error = r#"{"id":"1","error":{"code":"no_pane","message":"no pane"}}"#;
    let cases: Vec<(&str, Option<Vec<Herdr>>, usize)> = vec![
        ("no herdr socket", None, 0),
        ("no listener on the socket", Some(vec![]), 0),
        ("pane.current fails", Some(vec![Herdr::Say(error)]), 1),
        ("pane.current gives no pane", Some(vec![Herdr::Say(r#"{"id":"1","result":{"type":"pane_info"}}"#)]), 1),
        ("herdr closes with no reply", Some(vec![Herdr::Close]), 1),
        ("pane.send_input fails", Some(vec![Herdr::Say(PANE), Herdr::Say(error)]), 2),
        ("herdr hangs", Some(vec![Herdr::Hang]), 1),
    ];
    for (what, script, calls) in cases {
        let (reply, seen, paths) = herdr_press(script);
        assert_eq!(reply, (OK, format!("clip {paths}")), "{what}");
        assert_eq!(seen.len(), calls, "{what}");
    }
}

#[test]
fn the_herdr_socket_follows_the_config_folder_rule_of_herdr() {
    // `herdr_sock` reads the environment, and tests must not change it: check the rule on its
    // two inputs through the pure part.
    assert_eq!(remote::herdr_sock_in(Some("/x".into()), "/h".into()), Path::new("/x/herdr/herdr.sock"));
    assert_eq!(remote::herdr_sock_in(None, "/h".into()), Path::new("/h/.config/herdr/herdr.sock"));
}

// A Workstation core whose Remote has not said `HELLO` yet: the channel is up, but not ready.
fn late_remote() -> FakeRemote {
    let root = temp_root("late");
    let ex = root.join("ex");
    fs::create_dir_all(&ex).unwrap();
    let (a, s) = UnixStream::pair().unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (a2, ex2) = (a.try_clone().unwrap(), ex.clone());
    let ws = Some(std::thread::spawn(move || exchange::channel(a2, a, &ex2, "test-host", || {}, |_| {}, |_| false)));
    FakeRemote { root, ex, s, ws, last_ls: 1000 }
}

// The `HELLO` of the Remote, with `proto` as its protocol integer.
fn say_hello(r: &mut FakeRemote, proto: &str) {
    r.s.write_all(MAGIC).unwrap();
    r.s.write_all(&frame(0, HELLO, format!("{proto}\t0\tremote").as_bytes())).unwrap();
    let mut m = [0u8; 8];
    r.s.read_exact(&mut m).unwrap();
    assert_eq!(read_frame(&mut r.s).unwrap().1, HELLO);
}

// A press of one 3-byte file on its own thread. `connects` counts the calls of its `connect`.
fn late_press(ex: &Path, name: &str, wait: Duration, connects: &Arc<AtomicU32>) -> JoinHandle<Outcome> {
    let (ex, name, n) = (ex.to_path_buf(), name.to_string(), connects.clone());
    std::thread::spawn(move || {
        exchange::press(&ex, &[Item::mem(&name, b"abc".to_vec())], wait, move || { n.fetch_add(1, SeqCst); }, |_| panic!("clipboard"))
    })
}

#[test]
fn a_press_with_no_channel_connects_and_sends_its_copy_at_the_matching_hello() {
    let mut r = late_remote();
    let connects = Arc::new(AtomicU32::new(0));
    let h = late_press(&r.ex, "a.png", Duration::from_secs(5), &connects);
    wait_for("connect", || connects.load(SeqCst) == 1);
    assert!(!exchange::connected(&r.ex));
    say_hello(&mut r, PROTO);
    assert_eq!(r.next(), (1, OPEN, b"hotkey\t1\ta.png\t3".to_vec()));
    assert_eq!(r.next(), (1, DATA, b"abc".to_vec()));
    r.send(1, END, b"0\tpasted");
    assert_eq!(h.join().unwrap(), Outcome::Pasted);
    assert_eq!(connects.load(SeqCst), 1);
}

#[test]
fn a_press_connects_only_when_no_channel_is_up() {
    let r = FakeRemote::start();
    wait_for("channel", || exchange::connected(&r.ex));
    exchange::start_connect(&r.ex, || panic!("a channel is up"));
    let off = late_remote();
    let mut n = 0;
    exchange::start_connect(&off.ex, || n += 1);
    assert_eq!(n, 1);
}

#[test]
fn a_press_with_no_hello_in_time_gives_the_error_sound_and_drops_its_copy() {
    use crate::sounds::Sound::Error;
    let mut r = late_remote();
    let connects = Arc::new(AtomicU32::new(0));
    let before = asked(Error);
    let out = late_press(&r.ex, "a.png", Duration::from_millis(200), &connects).join().unwrap();
    assert!(matches!(&out, Outcome::Failed(w) if w.contains("no HELLO")), "{out:?}");
    assert!(asked(Error) > before);
    // The HELLO comes late: the old copy is not sent.
    say_hello(&mut r, PROTO);
    r.quiet();
}

#[test]
fn a_mismatched_hello_does_not_send_the_copy() {
    let mut r = late_remote();
    let connects = Arc::new(AtomicU32::new(0));
    let h = late_press(&r.ex, "a.png", Duration::from_millis(300), &connects);
    wait_for("connect", || connects.load(SeqCst) == 1);
    say_hello(&mut r, "1");
    assert!(r.ended().unwrap().starts_with("version mismatch"));
    assert!(matches!(h.join().unwrap(), Outcome::Failed(_)));
    assert!(!exchange::connected(&r.ex));
    assert!(read_frame(&mut r.s).is_err(), "no frame comes after the mismatch");
}

#[test]
fn a_newer_press_takes_over_from_one_that_waits_and_only_its_copy_is_sent() {
    let mut r = late_remote();
    let connects = Arc::new(AtomicU32::new(0));
    let old = late_press(&r.ex, "old.png", Duration::from_secs(5), &connects);
    wait_for("first connect", || connects.load(SeqCst) == 1);
    let new = late_press(&r.ex, "new.png", Duration::from_secs(5), &connects);
    // The old press returns when the new one has registered, long before its own limit.
    assert_eq!(old.join().unwrap(), Outcome::Superseded);
    say_hello(&mut r, PROTO);
    assert_eq!(r.next(), (1, OPEN, b"hotkey\t1\tnew.png\t3".to_vec()));
    assert_eq!(r.next().1, DATA);
    r.send(1, END, b"0\tpasted");
    assert_eq!(new.join().unwrap(), Outcome::Pasted);
    r.quiet();
}

// ---- herdr check ----

const PONG: &str = r#"{"id":"1","result":{"type":"pong","version":"0.9.5","protocol":22,"capabilities":{}}}"#;
const HOOK: &str = r#"{"on":"pane.agent_status_changed","command":["./ferry","hook"]}"#;

// A `plugin.list` reply with the given plugin objects.
fn plugin_list(plugins: &[String]) -> String {
    format!(r#"{{"id":"1","result":{{"type":"plugin_list","plugins":[{}]}}}}"#, plugins.join(","))
}

// The ferry plugin as `plugin.list` shows it. `extra` is more members of the object.
fn ferry_plugin(enabled: bool, events: &str, extra: &str) -> String {
    format!(r#"{{"plugin_id":"ferry","enabled":{enabled},"events":[{events}]{extra}}}"#)
}

// A herdr that answers each connection by its method. The replies can change while it runs. A
// method with no reply gets a closed connection. The second value counts the `ping` calls.
type Replies = Arc<Mutex<std::collections::HashMap<&'static str, String>>>;

fn herdr_daemon(sock: &Path, ping: &str, list: &str) -> (Replies, Arc<AtomicU32>) {
    use std::io::{BufRead, BufReader};
    let replies: Replies = Arc::default();
    replies.lock().unwrap().extend([("ping", ping.to_string()), ("plugin.list", list.to_string())]);
    replies.lock().unwrap().retain(|_, r| !r.is_empty());
    let listener = UnixListener::bind(sock).unwrap();
    let (r2, pings) = (replies.clone(), Arc::new(AtomicU32::new(0)));
    let p2 = pings.clone();
    std::thread::spawn(move || {
        for mut c in listener.incoming().map_while(Result::ok) {
            let mut line = String::new();
            let _ = BufReader::new(c.try_clone().unwrap()).read_line(&mut line);
            let method = ["ping", "plugin.list"].into_iter().find(|m| line.contains(&format!(r#""method":"{m}""#)));
            if method == Some("ping") {
                p2.fetch_add(1, SeqCst);
            }
            if let Some(r) = method.and_then(|m| r2.lock().unwrap().get(m).cloned()) {
                let _ = writeln!(c, "{r}");
            }
        }
    });
    (replies, pings)
}

// A `serve` that checks a fake herdr every 20 ms. Returns it with the fake herdr's controls.
fn herdr_ws(ping: &str, list: &str) -> (FakeWs, Replies, Arc<AtomicU32>) {
    let mut fake = None;
    let (ping, list) = (ping.to_string(), list.to_string());
    let w = FakeWs::launch("herdr.sock", Duration::from_millis(20), |sock| fake = Some(herdr_daemon(sock, &ping, &list)));
    let (replies, pings) = fake.unwrap();
    (w, replies, pings)
}

// The `HERDR` frame that comes next: its state and its text.
fn next_herdr(w: &mut FakeWs) -> (String, String) {
    let (id, kind, p) = w.next();
    assert_eq!((id, kind), (0, HERDR));
    let f = fields(&p);
    (f[0].clone(), f[1].clone())
}

#[test]
fn the_check_gives_ok_with_the_version_and_adds_the_tested_version_when_it_differs() {
    let list = plugin_list(&[ferry_plugin(true, HOOK, "")]);
    let (mut w, replies, _) = herdr_ws(PONG, &list);
    // This is the first frame after the HELLO: the check runs at the start.
    assert_eq!(next_herdr(&mut w), ("ok".into(), "0.9.5 · alert hook ok".into()));
    replies.lock().unwrap().insert("ping", PONG.replace("0.9.5", "0.9.6"));
    assert_eq!(next_herdr(&mut w), ("ok".into(), "0.9.6 (tested: 0.9.5) · alert hook ok".into()));
}

#[test]
fn the_check_fails_for_each_condition_of_the_spec_and_names_the_item() {
    let other = r#"{"on":"pane.updated","command":["./x"]}"#;
    let cases: Vec<(&str, String, &str)> = vec![
        ("plugin not linked", plugin_list(&[]), "not linked"),
        ("another plugin only", plugin_list(&[r#"{"plugin_id":"other","enabled":true,"events":[]}"#.into()]), "not linked"),
        ("plugin disabled", plugin_list(&[ferry_plugin(false, HOOK, "")]), "disabled"),
        ("no events", plugin_list(&[r#"{"plugin_id":"ferry","enabled":true}"#.into()]), "hook"),
        ("another event only", plugin_list(&[ferry_plugin(true, other, "")]), "hook"),
        ("unknown event warning", plugin_list(&[ferry_plugin(true, HOOK, r#","warnings":["unknown event 'pane.agent_done'"]"#)]), "unknown event"),
        ("warning text with JSON syntax in it", plugin_list(&[ferry_plugin(true, HOOK, r#","warnings":["unknown event '}{ \" ]'"]"#)]), "unknown event"),
        ("manifest unavailable warning", plugin_list(&[ferry_plugin(true, HOOK, r#","warnings":["manifest unavailable: gone"]"#)]), "manifest unavailable"),
        ("plugin.list method error", r#"{"id":"2","error":{"code":"unknown_method","message":"no such method"}}"#.into(), "no such method"),
    ];
    for (what, list, item) in cases {
        let (mut w, _, _) = herdr_ws(PONG, &list);
        let (state, text) = next_herdr(&mut w);
        assert_eq!(state, "failed", "{what}: {text}");
        assert!(text.contains(item), "{what}: {text}");
    }
    let (mut w, _, _) = herdr_ws(r#"{"id":"1","error":{"code":"internal","message":"broken"}}"#, &plugin_list(&[ferry_plugin(true, HOOK, "")]));
    let (state, text) = next_herdr(&mut w);
    assert!((state.as_str(), text.contains("broken")) == ("failed", true), "ping error: {state} {text}");
}

#[test]
fn the_check_gives_not_running_for_a_missing_socket_and_for_no_answer() {
    let list = plugin_list(&[ferry_plugin(true, HOOK, "")]);
    let mut missing = FakeWs::launch("no-herdr.sock", Duration::from_secs(60), |_| {});
    assert_eq!(next_herdr(&mut missing), ("not running".into(), "not running".into()));
    // The socket exists, but ping gets no reply. A reply that is no JSON is no answer either.
    for (what, ping, list) in [("no ping reply", "", list.as_str()), ("no JSON", "hello", list.as_str()), ("no plugin.list reply", PONG, "")] {
        let (mut w, _, _) = herdr_ws(ping, list);
        assert_eq!(next_herdr(&mut w).0, "not running", "{what}");
    }
}

#[test]
fn a_warning_that_is_not_a_failure_keeps_the_check_ok_and_is_logged_once() {
    let warn = "no platforms in the manifest";
    let list = plugin_list(&[ferry_plugin(true, HOOK, &format!(r#","warnings":["{warn}"]"#))]);
    let (mut w, _, pings) = herdr_ws(PONG, &list);
    assert_eq!(next_herdr(&mut w).0, "ok");
    wait_for("four checks", || pings.load(SeqCst) >= 4);
    let log = fs::read_to_string(w.state.join("serve.log")).unwrap();
    assert_eq!(log.matches(warn).count(), 1, "{log}");
}

#[test]
fn serve_sends_herdr_again_only_when_the_result_changes() {
    let list = plugin_list(&[ferry_plugin(true, HOOK, "")]);
    let (mut w, replies, pings) = herdr_ws(PONG, &list);
    assert_eq!(next_herdr(&mut w).0, "ok");
    // Several more checks with the same result send nothing.
    let n = pings.load(SeqCst);
    wait_for("more checks", || pings.load(SeqCst) >= n + 4);
    assert!(!frame_comes(&mut w.s));
    // herdr loses the plugin, and later the hook comes back.
    replies.lock().unwrap().insert("plugin.list", plugin_list(&[]));
    assert_eq!(next_herdr(&mut w).0, "failed");
    replies.lock().unwrap().insert("plugin.list", list);
    assert_eq!(next_herdr(&mut w).0, "ok");
    // herdr stops: the next result is `not running`.
    replies.lock().unwrap().clear();
    assert_eq!(next_herdr(&mut w).0, "not running");
}

fn herdr_link() -> (crate::link::Link, Arc<Mutex<Vec<crate::link::Report>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    (crate::link::Link::new("alias", "key", || "09:12".into(), move |r| s2.lock().unwrap().push(r.clone())), seen)
}

fn hello_ok(l: &crate::link::Link) {
    l.connecting();
    l.hello(&[PROTO.into(), "1.0.0".into()]);
}

fn herdr_line(l: &crate::link::Link) -> String {
    l.view().unwrap().0.herdr
}

fn fr(state: &str, text: &str) -> Vec<String> {
    vec![state.into(), text.into()]
}

#[test]
fn the_herdr_line_shows_each_form_and_only_failed_gives_the_red_dot() {
    use crate::link::Icon;
    let (l, _) = herdr_link();
    hello_ok(&l);
    assert_eq!(herdr_line(&l), "unknown");
    for (state, text, icon) in [
        ("ok", "0.9.5 · alert hook ok", Icon::Normal),
        ("ok", "0.9.6 (tested: 0.9.5) · alert hook ok", Icon::Normal),
        ("not running", "not running", Icon::Normal),
        ("failed", "alert hook missing", Icon::Alert),
    ] {
        l.herdr(&fr(state, text));
        assert_eq!(herdr_line(&l), text);
        assert_eq!(l.view().unwrap().1.icon, icon, "{state}");
    }
    assert_eq!(l.view().unwrap().1.tip, "ferry · herdr alert hook missing");
    // The line is unknown while the channel is down, and the red dot goes with it.
    l.ended("gone", "");
    assert_eq!(herdr_line(&l), "unknown (offline)");
    assert_eq!(l.view().unwrap().1.icon, Icon::Grey);
    // A new connection shows no old result until the new `HERDR` comes.
    hello_ok(&l);
    assert_eq!(herdr_line(&l), "unknown");
    assert_eq!(l.view().unwrap().1.icon, Icon::Normal);
}

#[test]
fn the_error_sound_is_due_once_when_the_state_changes_to_failed_also_across_reconnects() {
    let (l, _) = herdr_link();
    hello_ok(&l);
    // The first state of the process counts as a change.
    assert!(l.herdr(&fr("failed", "alert hook missing")));
    assert!(!l.herdr(&fr("failed", "alert hook missing")));
    assert!(!l.herdr(&fr("failed", "ferry plugin disabled")));
    // A reconnect with the same failure gives no sound.
    l.ended("gone", "");
    hello_ok(&l);
    assert!(!l.herdr(&fr("failed", "alert hook missing")));
    // Other states give none; a new failure after them does.
    assert!(!l.herdr(&fr("ok", "0.9.5 · alert hook ok")));
    assert!(!l.herdr(&fr("not running", "not running")));
    assert!(l.herdr(&fr("failed", "alert hook missing")));
    // `not running` is no failure at the first state of a process either.
    let (l, _) = herdr_link();
    hello_ok(&l);
    assert!(!l.herdr(&fr("not running", "not running")));
}

#[test]
fn a_herdr_frame_on_the_channel_reaches_the_link_and_a_failure_asks_for_the_error_sound() {
    use crate::sounds::Sound::Error;
    let root = temp_root("herdr-link");
    let (a, mut s) = UnixStream::pair().unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (link, _) = herdr_link();
    let (link, bells) = (Arc::new(link), Arc::new(AtomicU32::new(0)));
    let (a2, ex, l1, l2, b2) = (a.try_clone().unwrap(), root.clone(), link.clone(), link.clone(), bells.clone());
    let before = asked(Error);
    let ws = std::thread::spawn(move || {
        exchange::channel(a2, a, &ex, "test-host", || {}, |f| l1.hello(f), move |f| {
            let due = l2.herdr(f);
            b2.fetch_add(u32::from(due), SeqCst);
            due
        })
    });
    s.write_all(MAGIC).unwrap();
    s.write_all(&frame(0, HELLO, format!("{PROTO}\t0\tremote").as_bytes())).unwrap();
    for text in ["alert hook missing", "alert hook missing"] {
        s.write_all(&frame(0, HERDR, format!("failed\t{text}").as_bytes())).unwrap();
    }
    wait_for("the herdr line", || link.view().is_some_and(|v| v.0.herdr == "alert hook missing"));
    assert_eq!(bells.load(SeqCst), 1);
    assert!(asked(Error) > before);
    let _ = s.shutdown(std::net::Shutdown::Both);
    let _ = ws.join();
    let _ = fs::remove_dir_all(root);
}

// The old Workstation closes the channel as soon as it reads `BYE replaced`, before the
// replacement calls `quit`. The open agent request must still get the replaced text.
#[test]
fn a_takeover_gives_the_replaced_text_also_when_the_old_workstation_closes_at_once() {
    let mut w = FakeWs::start();
    let mut a = w.agent();
    a.write_all(&frame(1, OPEN, b"ls\t")).unwrap();
    assert_eq!(w.next().1, OPEN);
    let (mut ws2, b) = UnixStream::pair().unwrap();
    let (st, b2) = (w.state.clone(), b.try_clone().unwrap());
    let serve2 = std::thread::spawn(move || remote::serve_on(&st, b, b2, false));
    ws2.write_all(MAGIC).unwrap();
    ws2.write_all(&frame(0, HELLO, format!("{PROTO}\t0\tnew-host").as_bytes())).unwrap();
    while w.next().1 != BYE {}
    w.s.shutdown(std::net::Shutdown::Both).unwrap();
    let (_, kind, p) = read_frame(&mut a).unwrap();
    assert_eq!((kind, end_reply(&p)), (END, (OFFLINE, "replaced by a new connection".to_string())));
    ws2.shutdown(std::net::Shutdown::Both).unwrap();
    assert_eq!(serve2.join().unwrap(), 0);
}
