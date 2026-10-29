// The channel state of `ferry run`: the reconnect delays, the state that the user
// sees and the text of `ferry status`. It has no Win32 code. `exchange::run`
// drives it, `windows.rs` stores each change in the state file, and `ferry
// status` prints the stored text.

use crate::frame::{fields, read_frame, HELLO, MAGIC, PROTO};
use crate::lifecycle::State;
use std::io::{BufRead, ErrorKind, Read};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

// The reason of a connection that ends before the HELLO of the Remote.
pub const ENDED: &str = "ssh ended before HELLO";

// The waits after a failed connect. The last one repeats.
const WAITS: [u64; 5] = [5, 10, 20, 40, 60];

// The most login output that the Workstation skips before the magic.
const NOISE_MAX: usize = 4096;

#[derive(Default)]
pub struct Backoff(usize);

impl Backoff {
    // The wait for the next try, then the next longer one.
    pub fn take(&mut self) -> u64 {
        let s = WAITS[self.0];
        self.0 = (self.0 + 1).min(WAITS.len() - 1);
        s
    }

    pub fn reset(&mut self) {
        self.0 = 0;
    }
}

// Skips up to 4 KiB of login output, then reads the magic and the `HELLO` frame
// and returns its fields. `Err` is the reason that the connection cannot go on.
pub fn handshake(r: &mut impl BufRead) -> Result<Vec<String>, String> {
    let mut seen = Vec::new();
    for b in r.by_ref().bytes() {
        let Ok(b) = b else { break };
        seen.push(b);
        if seen.len() > NOISE_MAX + MAGIC.len() {
            return Err(format!("Remote login prints to stdout: {:?}", String::from_utf8_lossy(&seen[..80])));
        }
        if seen.ends_with(MAGIC) {
            break;
        }
    }
    if !seen.ends_with(MAGIC) {
        return Err(ENDED.into());
    }
    match read_frame(r) {
        Ok((_, HELLO, p)) => Ok(fields(&p)),
        Ok((_, k, _)) => Err(format!("expected HELLO, got frame kind {k}")),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Err(ENDED.into()),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Chan {
    Connecting,
    Connected { since: String, version: String },
    Offline { since: String, detail: String, wait: u64 },
    Mismatch { remote: String, wait: u64 },
    Replaced { host: String },
}

impl Chan {
    // The text after `channel: ` in `ferry status`.
    pub fn text(&self, alias: &str) -> String {
        match self {
            Chan::Connecting => format!("connecting to {alias}"),
            Chan::Connected { since, version } => format!("connected to {alias} since {since} · Remote ferry {version}"),
            Chan::Offline { since, detail, wait } => format!("offline since {since}: {detail} · next try in {wait} s"),
            Chan::Mismatch { remote, wait } => {
                // A remote integer that is not a number counts as older.
                let older = if remote.parse::<u32>().unwrap_or(0) < PROTO.parse().unwrap_or(0) { "Remote" } else { "Workstation" };
                format!("version mismatch: Workstation {PROTO}, Remote {remote} · upgrade the {older} · next try in {wait} s")
            }
            Chan::Replaced { host } => format!("replaced by {host} · press the hotkey to connect again and send"),
        }
    }
}

// What `ferry run` stores in the state file after the first line, and what
// `ferry status` prints. Each text is the part after its label.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    // `HH:MM` of the process start and of this change.
    pub since: String,
    pub changed: String,
    pub connected: bool,
    pub channel: String,
    pub herdr: String,
    pub hotkey: String,
}

impl Report {
    pub fn text(&self) -> String {
        format!("{}\t{}\t{}\n{}\n{}\n{}\n", self.since, self.changed, u8::from(self.connected), self.channel, self.herdr, self.hotkey)
    }

    pub fn parse(text: &str) -> Option<Report> {
        let mut l = text.lines();
        let mut f = l.next()?.split('\t');
        Some(Report {
            since: f.next()?.into(),
            changed: f.next()?.into(),
            connected: f.next()? == "1",
            channel: l.next()?.into(),
            herdr: l.next()?.into(),
            hotkey: l.next()?.into(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Icon {
    Normal,
    Grey,
    // Normal with a red dot: the user must act.
    Alert,
}

// What the tray icon shows: the icon and the hover line.
#[derive(Clone, Debug, PartialEq)]
pub struct Tray {
    pub icon: Icon,
    pub tip: String,
}

// The most UTF-16 units of a hover line; the Win32 `szTip` holds 128 with its end mark.
const TIP_MAX: usize = 127;

// Whole minutes from `since` to `now`, both `HH:MM`. ponytail: wraps after 24 h; the
// hover then shows a short time until the next state change.
fn minutes(since: &str, now: &str) -> u32 {
    let m = |t: &str| t.split_once(':').and_then(|(h, m)| Some(h.parse::<u32>().ok()? * 60 + m.parse::<u32>().ok()?)).unwrap_or(0);
    (m(now) + 1440 - m(since)) % 1440
}

// The icon and the hover line of a channel state. `now` is the local `HH:MM`.
pub fn tray(chan: &Chan, alias: &str, now: &str) -> Tray {
    let state = match chan {
        Chan::Connecting => format!("connecting to {alias}"),
        Chan::Connected { .. } => format!("connected to {alias}"),
        Chan::Offline { since, detail, .. } => match minutes(since, now) {
            0 => format!("offline: {detail}"),
            n => format!("offline {n} min: {detail}"),
        },
        Chan::Mismatch { .. } | Chan::Replaced { .. } => chan.text(alias),
    };
    // The reasons that need the user to act, worst first. A new reason is one more line here.
    let mut act: Vec<&str> = Vec::new();
    if matches!(chan, Chan::Mismatch { .. } | Chan::Replaced { .. }) {
        act.push(&state);
    }
    let icon = match (act.is_empty(), chan) {
        (false, _) => Icon::Alert,
        (true, Chan::Connected { .. }) => Icon::Normal,
        (true, _) => Icon::Grey,
    };
    let line = format!("ferry · {}", act.first().copied().unwrap_or(&state)).replace(['\r', '\n'], " ");
    let (mut tip, mut units) = (String::new(), 0);
    for c in line.chars() {
        units += c.len_utf16();
        if units > TIP_MAX {
            break;
        }
        tip.push(c);
    }
    Tray { icon, tip }
}

#[derive(Clone, Copy, PartialEq)]
enum Kick {
    Reset,
    Now,
}

struct Inner {
    chan: Chan,
    backoff: Backoff,
    // `HH:MM` of the first failure since the last connection.
    down: Option<String>,
    // The protocol integer of a mismatched `HELLO` on this connection.
    mismatch: Option<String>,
    kick: Option<Kick>,
    // The report and the tray view of the last change.
    last: Option<(Report, Tray)>,
}

// The state of the channel. The connect loop changes it; the hotkey and the
// window of the process wake it.
pub struct Link {
    alias: String,
    hotkey: String,
    since: String,
    clock: fn() -> String,
    sink: Box<dyn Fn(&Report) + Send + Sync>,
    m: Mutex<Inner>,
    cv: Condvar,
}

impl Link {
    // `hotkey` is the text after `hotkey: `. `clock` gives the local `HH:MM`.
    // `sink` gets the report at each state change.
    pub fn new(alias: &str, hotkey: &str, clock: fn() -> String, sink: impl Fn(&Report) + Send + Sync + 'static) -> Link {
        let m = Inner { chan: Chan::Connecting, backoff: Backoff::default(), down: None, mismatch: None, kick: None, last: None };
        Link { alias: alias.into(), hotkey: hotkey.into(), since: clock(), clock, sink: Box::new(sink), m: Mutex::new(m), cv: Condvar::new() }
    }

    fn set(&self, g: &mut Inner, chan: Chan) {
        let connected = matches!(chan, Chan::Connected { .. });
        let now = (self.clock)();
        g.chan = chan;
        let rep = Report {
            since: self.since.clone(),
            changed: now.clone(),
            connected,
            channel: g.chan.text(&self.alias).replace(['\r', '\n'], " "),
            herdr: if connected { "unknown" } else { "unknown (offline)" }.into(),
            hotkey: self.hotkey.clone(),
        };
        (self.sink)(&rep);
        g.last = Some((rep, tray(&g.chan, &self.alias, &now)));
    }

    // The report and the tray view of the last change, or `None` before the first.
    pub fn view(&self) -> Option<(Report, Tray)> {
        self.m.lock().unwrap().last.clone()
    }

    #[cfg(test)]
    pub fn chan(&self) -> Chan {
        self.m.lock().unwrap().chan.clone()
    }

    // A connect starts.
    pub fn connecting(&self) {
        let mut g = self.m.lock().unwrap();
        g.kick = None;
        g.mismatch = None;
        self.set(&mut g, Chan::Connecting);
    }

    // The `HELLO` of the Remote. A matching protocol integer connects the channel
    // and sets the next wait back to 5 s; a mismatched one changes nothing but
    // the reason of the end.
    pub fn hello(&self, f: &[String]) {
        let mut g = self.m.lock().unwrap();
        if f[0] == PROTO {
            g.backoff.reset();
            g.down = None;
            let chan = Chan::Connected { since: (self.clock)(), version: f.get(1).cloned().unwrap_or_default() };
            self.set(&mut g, chan);
        } else {
            g.mismatch = Some(f[0].clone());
        }
    }

    // The connection ended with `why`; `last` is the last stderr line of
    // `ssh.exe`, or empty. Returns the wait before the next try, or `None` after
    // `BYE replaced`: no try then, until `connect_now`.
    pub fn ended(&self, why: &str, last: &str) -> Option<u64> {
        let mut g = self.m.lock().unwrap();
        if let Some(host) = why.strip_prefix("replaced\t") {
            self.set(&mut g, Chan::Replaced { host: host.into() });
            return None;
        }
        let wait = g.backoff.take();
        let chan = match g.mismatch.take() {
            Some(remote) => Chan::Mismatch { remote, wait },
            None => {
                let since = g.down.get_or_insert_with(self.clock).clone();
                let detail = match (why == ENDED, last.is_empty()) {
                    (true, true) => why.into(),
                    (true, false) => format!("{why}: {last}"),
                    (false, true) => why.into(),
                    (false, false) => last.into(),
                };
                Chan::Offline { since, detail, wait }
            }
        };
        self.set(&mut g, chan);
        Some(wait)
    }

    // Waits `secs` before the next try, or without end for `None`. `connect_now`
    // ends the wait at once; `reset` cuts it to the first delay.
    pub fn pause(&self, secs: Option<u64>) {
        let mut g = self.m.lock().unwrap();
        let mut until = secs.map(|s| Instant::now() + Duration::from_secs(s));
        loop {
            match g.kick.take() {
                Some(Kick::Now) => return,
                Some(Kick::Reset) => until = until.map(|u| u.min(Instant::now() + Duration::from_secs(WAITS[0]))),
                None => {}
            }
            g = match until {
                None => self.cv.wait(g).unwrap(),
                Some(u) => {
                    let left = u.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return;
                    }
                    self.cv.wait_timeout(g, left).unwrap().0
                }
            };
        }
    }

    // The PC woke from sleep: the next wait is 5 s again. A channel that waits
    // after `BYE replaced` stays as it is.
    pub fn reset(&self) {
        let mut g = self.m.lock().unwrap();
        g.backoff.reset();
        g.kick = g.kick.or(Some(Kick::Reset));
        self.cv.notify_all();
    }

    // Connect at once, also after `BYE replaced`.
    #[allow(dead_code)]
    pub fn connect_now(&self) {
        let mut g = self.m.lock().unwrap();
        g.kick = Some(Kick::Now);
        self.cv.notify_all();
    }
}

// The text that `ferry status` prints, and its exit code: 0 only when `ferry
// run` runs and its channel is connected. `st` is the first line of the state
// file, `running` tells if that process still lives, `rep` is the rest of the
// file, and `log` is the path of the log.
pub fn status_text(this: &str, st: Option<&State>, running: bool, rep: Option<&Report>, log: &str) -> (String, i32) {
    let head = match st {
        Some(s) if running => {
            let since = rep.map_or("?", |r| r.since.as_str());
            let who = if s.version == this { format!("ferry {this} · running") } else { format!("ferry {this} (this exe) · running ferry {}", s.version) };
            format!("{who}, pid {}, since {since}", s.pid)
        }
        Some(s) => {
            let last = rep.map_or(String::new(), |r| format!(", last state {}", r.changed));
            format!("ferry {this} · not running (last pid {}{last}) · run: ferry start <alias>", s.pid)
        }
        None => format!("ferry {this} · not running · run: ferry start <alias>"),
    };
    if !running {
        return (format!("{head}\nlog: {log}\n"), 1);
    }
    let (channel, herdr, hotkey) = rep.map_or(("starting", "unknown", "unknown"), |r| (r.channel.as_str(), r.herdr.as_str(), r.hotkey.as_str()));
    (format!("{head}\nchannel: {channel}\nherdr: {herdr}\nhotkey: {hotkey}\nlog: {log}\n"), i32::from(!rep.is_some_and(|r| r.connected)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::frame;
    use std::io::Cursor;
    use std::sync::Arc;

    fn clock() -> String {
        "09:12".into()
    }

    fn hello_frame(payload: &str) -> Vec<u8> {
        let mut v = MAGIC.to_vec();
        v.extend(frame(0, HELLO, payload.as_bytes()));
        v
    }

    fn shake(bytes: Vec<u8>) -> Result<Vec<String>, String> {
        handshake(&mut Cursor::new(bytes))
    }

    #[test]
    fn backoff_doubles_to_a_limit_and_resets() {
        let mut b = Backoff::default();
        let seq: Vec<u64> = (0..8).map(|_| b.take()).collect();
        assert_eq!(seq, [5, 10, 20, 40, 60, 60, 60, 60]);
        b.reset();
        assert_eq!(b.take(), 5);
    }

    #[test]
    fn handshake_skips_up_to_4_kib_of_login_output() {
        for noise in [0, 1, 4096] {
            let mut v = vec![b'x'; noise];
            v.extend(hello_frame("2\t1.0.0\thost"));
            assert_eq!(shake(v).unwrap(), ["2", "1.0.0", "host"], "{noise} bytes");
        }
    }

    #[test]
    fn handshake_refuses_more_than_4_kib_of_login_output() {
        let mut v = b"Welcome to the box\n".to_vec();
        v.resize(4097, b' ');
        v.extend(hello_frame("2\t1.0.0\thost"));
        let e = shake(v).unwrap_err();
        assert!(e.starts_with("Remote login prints to stdout: \"Welcome to the box"), "{e}");
    }

    #[test]
    fn handshake_reports_an_end_before_the_hello() {
        for bytes in [vec![], b"banner".to_vec(), MAGIC.to_vec(), [&MAGIC[..], &[0, 0, 0][..]].concat()] {
            assert_eq!(shake(bytes).unwrap_err(), ENDED);
        }
    }

    #[test]
    fn handshake_wants_a_hello_first() {
        let mut v = MAGIC.to_vec();
        v.extend(frame(0, crate::frame::PING, b""));
        assert!(shake(v).unwrap_err().starts_with("expected HELLO"));
        let mut v = MAGIC.to_vec();
        v.extend([0, 1, 0, 1, 0, 0, 0, 0, HELLO]);
        assert!(shake(v).unwrap_err().contains("frame of"));
    }

    fn link() -> (Arc<Link>, Arc<Mutex<Vec<Report>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        (Arc::new(Link::new("alias", "Ctrl+Alt+V for rio.exe", clock, move |r| s2.lock().unwrap().push(Report::parse(&r.text()).unwrap()))), seen)
    }

    fn hello(l: &Link, proto: &str) {
        l.hello(&[proto.into(), "1.0.0".into(), "host".into()]);
    }

    #[test]
    fn a_matching_hello_resets_the_wait_and_a_mismatched_one_does_not() {
        let (l, _) = link();
        assert_eq!([l.ended("x", ""), l.ended("x", ""), l.ended("x", "")], [Some(5), Some(10), Some(20)]);
        hello(&l, "2");
        assert_eq!(l.ended("channel: eof", ""), Some(5));
        l.ended("x", "");
        hello(&l, "1");
        assert_eq!(l.ended("version mismatch", ""), Some(20));
        hello(&l, "1");
        assert_eq!(l.ended("version mismatch", ""), Some(40));
        assert_eq!(l.chan(), Chan::Mismatch { remote: "1".into(), wait: 40 });
    }

    #[test]
    fn a_mismatch_names_the_older_side() {
        let (l, _) = link();
        for (remote, older) in [("1", "Remote"), ("3", "Workstation"), ("v", "Remote")] {
            hello(&l, remote);
            l.ended("version mismatch", "");
            assert!(l.chan().text("alias").contains(&format!("Workstation 2, Remote {remote} · upgrade the {older} · next try in")), "{remote}");
        }
    }

    #[test]
    fn replaced_stops_the_tries_until_connect_now() {
        let (l, _) = link();
        assert_eq!(l.ended("replaced\tother-host", ""), None);
        assert_eq!(l.chan(), Chan::Replaced { host: "other-host".into() });
        let l2 = l.clone();
        let t = std::thread::spawn(move || l2.pause(None));
        std::thread::sleep(Duration::from_millis(50));
        l.reset();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!t.is_finished(), "a wake from sleep must not reconnect");
        l.connect_now();
        t.join().unwrap();
    }

    #[test]
    fn connect_now_ends_a_wait() {
        let (l, _) = link();
        let l2 = l.clone();
        let t = std::thread::spawn(move || l2.pause(Some(3600)));
        std::thread::sleep(Duration::from_millis(50));
        l.connect_now();
        t.join().unwrap();
    }

    #[test]
    fn a_connect_that_starts_drops_an_old_connect_now() {
        let (l, _) = link();
        l.connect_now();
        l.connecting();
        l.pause(Some(0));
        assert!(l.m.lock().unwrap().kick.is_none());
    }

    #[test]
    fn offline_keeps_its_start_time_over_the_retries() {
        fn tick() -> String {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            format!("t{}", N.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
        }
        let l = Link::new("a", "k", tick, |_| {});
        let since = |l: &Link| match l.chan() {
            Chan::Offline { since, .. } => since,
            c => panic!("{c:?}"),
        };
        l.ended("x", "");
        let first = since(&l);
        l.connecting();
        l.ended("y", "");
        assert_eq!(since(&l), first);
        hello(&l, "2");
        l.ended("z", "");
        assert_ne!(since(&l), first, "a connection in between starts a new outage");
    }

    #[test]
    fn channel_text_of_each_state() {
        let (l, seen) = link();
        l.connecting();
        hello(&l, "2");
        l.ended(ENDED, "Permission denied (publickey).");
        l.ended("channel: eof", "Connection closed by 10.0.0.1");
        l.ended("FAIL: spawn: not found", "");
        hello(&l, "1");
        l.ended("version mismatch", "");
        l.ended("replaced\tbox", "");
        let t: Vec<String> = seen.lock().unwrap().iter().map(|r| r.channel.clone()).collect();
        assert_eq!(
            t,
            [
                "connecting to alias",
                "connected to alias since 09:12 · Remote ferry 1.0.0",
                "offline since 09:12: ssh ended before HELLO: Permission denied (publickey). · next try in 5 s",
                "offline since 09:12: Connection closed by 10.0.0.1 · next try in 10 s",
                "offline since 09:12: FAIL: spawn: not found · next try in 20 s",
                "version mismatch: Workstation 2, Remote 1 · upgrade the Remote · next try in 40 s",
                "replaced by box · press the hotkey to connect again and send",
            ]
        );
    }

    #[test]
    fn the_herdr_line_is_unknown_unless_connected() {
        let (l, seen) = link();
        l.connecting();
        hello(&l, "2");
        l.ended("x", "");
        let h: Vec<(bool, String)> = seen.lock().unwrap().iter().map(|r| (r.connected, r.herdr.clone())).collect();
        assert_eq!(h, [(false, "unknown (offline)".into()), (true, "unknown".into()), (false, "unknown (offline)".into())]);
    }

    #[test]
    fn a_report_survives_the_state_file() {
        let r = Report { since: "09:12".into(), changed: "09:40".into(), connected: true, channel: "a · b".into(), herdr: "unknown".into(), hotkey: "Ctrl+Alt+V for rio.exe".into() };
        assert_eq!(Report::parse(&r.text()), Some(r));
        assert_eq!(Report::parse("09:12\t09:40"), None);
        let l = Link::new("a", "k", clock, |r| assert!(!r.channel.contains('\n')));
        l.ended("two\nlines", "");
    }

    fn state(version: &str) -> State {
        State { pid: 20892, start: 1, version: version.into() }
    }

    fn report(channel: &str, connected: bool) -> Report {
        Report { since: "09:12".into(), changed: "09:40".into(), connected, channel: channel.into(), herdr: "unknown (offline)".into(), hotkey: "Ctrl+Alt+V for rio.exe".into() }
    }

    #[test]
    fn status_connected_has_five_lines_and_exit_0() {
        let (t, code) = status_text("1.0.0", Some(&state("1.0.0")), true, Some(&report("connected to a since 09:12 · Remote ferry 1.0.0", true)), r"C:\ferry.log");
        assert_eq!(
            t,
            "ferry 1.0.0 · running, pid 20892, since 09:12\nchannel: connected to a since 09:12 · Remote ferry 1.0.0\nherdr: unknown (offline)\nhotkey: Ctrl+Alt+V for rio.exe\nlog: C:\\ferry.log\n"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn status_exits_1_unless_connected() {
        for c in ["connecting to a", "offline since 09:12: x · next try in 5 s", "replaced by b · press the hotkey to connect again and send"] {
            let (t, code) = status_text("1.0.0", Some(&state("1.0.0")), true, Some(&report(c, false)), "log");
            assert_eq!((t.lines().count(), t.contains(&format!("channel: {c}\n")), code), (5, true, 1), "{c}");
        }
        let (t, code) = status_text("1.0.0", Some(&state("1.0.0")), true, None, "log");
        assert_eq!((t.lines().count(), code), (5, 1));
    }

    #[test]
    fn status_when_ferry_does_not_run() {
        let (t, code) = status_text("1.0.0", Some(&state("1.0.0")), false, Some(&report("connected", true)), "log");
        assert_eq!(t, "ferry 1.0.0 · not running (last pid 20892, last state 09:40) · run: ferry start <alias>\nlog: log\n");
        assert_eq!(code, 1);
        let (t, code) = status_text("1.0.0", None, false, None, "log");
        assert_eq!(t, "ferry 1.0.0 · not running · run: ferry start <alias>\nlog: log\n");
        assert_eq!(code, 1);
    }

    #[test]
    fn status_names_both_versions_when_they_differ() {
        let (t, _) = status_text("1.1.0", Some(&state("1.0.0")), true, Some(&report("c", true)), "log");
        assert_eq!(t.lines().next().unwrap(), "ferry 1.1.0 (this exe) · running ferry 1.0.0, pid 20892, since 09:12");
    }

    fn offline(since: &str, detail: &str) -> Chan {
        Chan::Offline { since: since.into(), detail: detail.into(), wait: 5 }
    }

    #[test]
    fn the_icon_follows_the_channel() {
        let c = Chan::Connected { since: "09:12".into(), version: "1.0.0".into() };
        let icon = |c: &Chan| tray(c, "a", "09:30").icon;
        assert_eq!(icon(&c), Icon::Normal);
        assert_eq!(icon(&Chan::Connecting), Icon::Grey);
        assert_eq!(icon(&offline("09:12", "x")), Icon::Grey);
        assert_eq!(icon(&Chan::Mismatch { remote: "1".into(), wait: 5 }), Icon::Alert);
        assert_eq!(icon(&Chan::Replaced { host: "b".into() }), Icon::Alert);
    }

    #[test]
    fn the_hover_names_the_state_and_the_time_offline() {
        let tip = |c: &Chan| tray(c, "work", "09:24").tip;
        assert_eq!(tip(&Chan::Connected { since: "09:12".into(), version: "1.0.0".into() }), "ferry · connected to work");
        assert_eq!(tip(&Chan::Connecting), "ferry · connecting to work");
        assert_eq!(tip(&offline("09:12", "Could not resolve hostname")), "ferry · offline 12 min: Could not resolve hostname");
        assert_eq!(tip(&offline("09:24", "x")), "ferry · offline: x");
        assert_eq!(tip(&offline("23:50", "x")), "ferry · offline 574 min: x");
        assert_eq!(tip(&offline("09:50", "x")), "ferry · offline 1414 min: x");
    }

    #[test]
    fn the_hover_of_a_user_action_is_the_channel_line() {
        for c in [Chan::Mismatch { remote: "1".into(), wait: 20 }, Chan::Replaced { host: "other".into() }] {
            assert_eq!(tray(&c, "work", "09:24").tip, format!("ferry · {}", c.text("work")));
        }
    }

    #[test]
    fn the_hover_fits_the_win32_limit() {
        // 127 UTF-16 units at most, and never a cut inside a character.
        for detail in ["x".repeat(500), "é".repeat(500), "😀".repeat(500), format!("a\nb\r\n{}", "y".repeat(300))] {
            let tip = tray(&offline("09:12", &detail), "work", "09:24").tip;
            let units = tip.encode_utf16().count();
            assert!((126..=127).contains(&units), "{units}");
            assert!(!tip.contains(['\r', '\n']));
        }
    }

    #[test]
    fn the_link_keeps_the_last_view_for_the_tray() {
        let (l, _) = link();
        assert_eq!(l.view(), None);
        l.connecting();
        assert_eq!(l.view().unwrap().1, Tray { icon: Icon::Grey, tip: "ferry · connecting to alias".into() });
        hello(&l, PROTO);
        let (r, t) = l.view().unwrap();
        assert!(r.connected);
        assert_eq!(t.icon, Icon::Normal);
    }
}
