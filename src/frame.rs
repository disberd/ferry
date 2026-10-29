// Frame codec and fair writer of the channel protocol. Both sides use it:
// the Workstation (`exchange`) and the Remote (`remote`).
//
// Frame: u32 payload length, u32 stream id, u8 kind (big-endian), payload.
// The largest payload is CHUNK. Control payloads are UTF-8 fields with a tab
// between them. On the ssh.exe pipes, each side writes MAGIC first, then its
// HELLO frame.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAGIC: &[u8; 8] = b"\0FERRY\r\n";
pub const PROTO: &str = "2";
pub const CHUNK: usize = 64 * 1024;
pub const CAP: u64 = 512 << 20; // largest single transfer
pub const PING_S: u64 = 15;
pub const DEAD_MS: u64 = 45_000;

pub const HELLO: u8 = 1;
pub const OPEN: u8 = 2;
pub const DATA: u8 = 3;
pub const END: u8 = 4;
pub const CANCEL: u8 = 5;
pub const PING: u8 = 6;
#[cfg(unix)]
pub const TAKEOVER: u8 = 7;
pub const BYE: u8 = 8;
pub const ALERT: u8 = 9;

// Exit codes of the agent commands. END carries them.
pub const OK: u8 = 0;
pub const BAD_PATH: u8 = 2;
#[cfg(unix)]
pub const OFFLINE: u8 = 3;
pub const IN_USE: u8 = 4;
pub const TOO_LARGE: u8 = 5;

pub fn frame(id: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(9 + payload.len());
    f.extend((payload.len() as u32).to_be_bytes());
    f.extend(id.to_be_bytes());
    f.push(kind);
    f.extend(payload);
    f
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u32, u8, Vec<u8>)> {
    let mut h = [0u8; 9];
    r.read_exact(&mut h)?;
    let len = u32::from_be_bytes(h[0..4].try_into().unwrap()) as usize;
    if len > CHUNK {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} B")));
    }
    let mut p = vec![0; len];
    r.read_exact(&mut p)?;
    Ok((u32::from_be_bytes(h[4..8].try_into().unwrap()), h[8], p))
}

pub fn fields(p: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(p).split('\t').map(String::from).collect()
}

pub fn end(code: u8, msg: &str) -> Vec<u8> {
    format!("{code}\t{msg}").into_bytes()
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

// One writer for one pipe. Control frames go first. After that, one frame from
// each stream in turn, so a 100 MB transfer delays an `ls` reply by one chunk.
// A stream holds at most DEPTH frames; its producer waits for the pipe.
#[derive(Default)]
pub struct Out {
    q: Mutex<Q>,
    cv: Condvar,
}

#[derive(Default)]
struct Q {
    ctrl: VecDeque<Vec<u8>>,
    streams: VecDeque<(u32, VecDeque<Vec<u8>>)>,
    dead: bool,
}

const DEPTH: usize = 4;

impl Out {
    pub fn ctrl(&self, id: u32, kind: u8, p: &[u8]) {
        let mut q = self.q.lock().unwrap();
        if kind == CANCEL {
            q.streams.retain(|s| s.0 != id);
        }
        q.ctrl.push_back(frame(id, kind, p));
        self.cv.notify_all();
    }

    // DATA and END: in order within the stream. False when the pipe is dead.
    pub fn stream(&self, id: u32, kind: u8, p: &[u8]) -> bool {
        let f = frame(id, kind, p);
        let mut q = self.q.lock().unwrap();
        loop {
            if q.dead {
                return false;
            }
            match q.streams.iter().position(|s| s.0 == id) {
                Some(i) if q.streams[i].1.len() >= DEPTH => q = self.cv.wait(q).unwrap(),
                Some(i) => {
                    q.streams[i].1.push_back(f);
                    break;
                }
                None => {
                    q.streams.push_back((id, VecDeque::from([f])));
                    break;
                }
            }
        }
        self.cv.notify_all();
        true
    }

    pub fn kill(&self) {
        self.q.lock().unwrap().dead = true;
        self.cv.notify_all();
    }

    pub fn dead(&self) -> bool {
        self.q.lock().unwrap().dead
    }

    pub fn run(&self, mut w: impl Write) {
        loop {
            let f = {
                let mut q = self.q.lock().unwrap();
                loop {
                    if q.dead {
                        return;
                    }
                    if let Some(f) = q.ctrl.pop_front() {
                        break f;
                    }
                    if let Some((id, mut fs)) = q.streams.pop_front() {
                        let f = fs.pop_front().unwrap();
                        if !fs.is_empty() {
                            q.streams.push_back((id, fs));
                        }
                        break f;
                    }
                    q = self.cv.wait(q).unwrap();
                }
            };
            self.cv.notify_all();
            if w.write_all(&f).and_then(|_| w.flush()).is_err() {
                return self.kill();
            }
        }
    }
}
