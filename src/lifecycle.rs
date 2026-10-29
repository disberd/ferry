// The parts of the process lifecycle that do not need Win32: the options of
// `ferry start` and `ferry run`, the state file text, the log files and the
// panic hook. `windows.rs` and `remote.rs` call them.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_HOTKEY: &str = "Ctrl+Alt+V";
pub const DEFAULT_APPS: &str = "rio.exe";

// Win32 modifier bits of `RegisterHotKey`.
pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

// A log file that grows past this size moves to `<name>.1`.
const LOG_MAX: u64 = 1 << 20;

// The checked arguments of `ferry start` and `ferry run`.
#[derive(Debug, PartialEq)]
pub struct Opts {
    pub alias: String,
    // The hotkey in canonical text, for example `Ctrl+Alt+V`.
    pub hotkey: String,
    pub mods: u32,
    pub vk: u32,
    // Lower case exe names.
    pub apps: Vec<String>,
}

impl Opts {
    // The arguments that `ferry start` gives to `ferry run`.
    pub fn run_args(&self) -> Vec<String> {
        vec!["run".into(), self.alias.clone(), "--hotkey".into(), self.hotkey.clone(), "--apps".into(), self.apps.join(",")]
    }
}

// `args` follow the command name: `<alias> [--hotkey <key>] [--apps <exe>,<exe>]`.
pub fn parse(args: &[&str]) -> Result<Opts, String> {
    let mut it = args.iter().copied();
    let alias = match it.next() {
        Some(a) if !a.is_empty() && !a.starts_with('-') => a.to_string(),
        _ => return Err("the <alias> is missing".into()),
    };
    let (mut hotkey, mut apps) = (DEFAULT_HOTKEY, DEFAULT_APPS);
    while let Some(opt) = it.next() {
        let v = it.next().ok_or_else(|| format!("{opt} needs a value"))?;
        match opt {
            "--hotkey" => hotkey = v,
            "--apps" => apps = v,
            _ => return Err(format!("unknown option {opt}")),
        }
    }
    let (hotkey, mods, vk) = parse_hotkey(hotkey)?;
    Ok(Opts { alias, hotkey, mods, vk, apps: parse_apps(apps)? })
}

// Modifiers (`Ctrl`, `Alt`, `Shift`, `Win`) joined by `+`, then one letter,
// one digit or `F1` to `F24`. Returns the canonical text, the modifier bits and
// the virtual key code.
fn parse_hotkey(s: &str) -> Result<(String, u32, u32), String> {
    let bad = |why: String| format!("bad --hotkey {s:?}: {why}");
    let (key, mods) = s.rsplit_once('+').map_or((s, ""), |(m, k)| (k, m));
    let mut bits = 0;
    for m in mods.split('+').filter(|_| !mods.is_empty()) {
        let bit = match m.to_ascii_lowercase().as_str() {
            "ctrl" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" => MOD_WIN,
            _ => return Err(bad(format!("{m:?} is not a modifier (Ctrl, Alt, Shift, Win)"))),
        };
        if bits & bit != 0 {
            return Err(bad(format!("{m:?} is twice in the list")));
        }
        bits |= bit;
    }
    let key = key.to_ascii_uppercase();
    let fnum = key.strip_prefix('F').filter(|n| !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit()));
    let vk = match (key.as_bytes(), fnum.and_then(|n| n.parse::<u32>().ok())) {
        (&[c], _) if c.is_ascii_alphanumeric() => c as u32,
        (_, Some(n @ 1..=24)) => 0x70 + n - 1,
        _ => return Err(bad(format!("{key:?} is not one letter, one digit or F1 to F24"))),
    };
    let mut text = String::new();
    for (bit, name) in [(MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Win")] {
        if bits & bit != 0 {
            text += name;
            text += "+";
        }
    }
    Ok((text + &key, bits, vk))
}

// Comma-separated exe names, at least one. The case does not matter.
fn parse_apps(s: &str) -> Result<Vec<String>, String> {
    let apps: Vec<String> = s.split(',').map(|a| a.trim().to_ascii_lowercase()).collect();
    for a in &apps {
        let name = a.strip_suffix(".exe").unwrap_or("");
        if name.is_empty() || name.contains(|c: char| c.is_control() || "\\/:*?\"<>|".contains(c)) {
            return Err(format!("bad --apps {s:?}: {a:?} is not an exe name such as rio.exe"));
        }
    }
    Ok(apps)
}

// The state file: the first line holds the pid, the start time of the process
// (the `FILETIME` of its creation) and the build version, separated by TAB.
// Later lines belong to the status text.
pub struct State {
    pub pid: u32,
    pub start: u64,
    pub version: String,
}

impl State {
    pub fn text(&self) -> String {
        format!("{}\t{}\t{}\n", self.pid, self.start, self.version)
    }

    pub fn parse(text: &str) -> Option<State> {
        let mut f = text.lines().next()?.split('\t');
        Some(State { pid: f.next()?.parse().ok()?, start: f.next()?.parse().ok()?, version: f.next()?.to_string() })
    }
}

// Moves `path` to `<path>.1` when it is larger than 1 MiB, and replaces the old
// copy. Errors are ignored: a log must not stop the process.
pub fn rotate(path: &Path) {
    if fs::metadata(path).is_ok_and(|m| m.len() > LOG_MAX) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = fs::rename(path, old);
    }
}

// Appends `text` to `path`. Errors are ignored.
pub fn append(path: &Path, text: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

// One line with a UTC time stamp, after the size check of `rotate`.
pub fn log_line(path: &Path, msg: &str) {
    rotate(path);
    append(path, &format!("{} {msg}\n", timestamp()));
}

// UTC `YYYY-MM-DD HH:MM:SS` from the system clock (days to date: Hinnant's civil_from_days).
fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

// Gives the panic message and its source location, on one line, to `sink`. The
// release build aborts after the hook, and a detached process has no stderr, so
// the hook is the only trace of a crash.
pub fn panic_hook(sink: impl Fn(&str) + Send + Sync + 'static) {
    std::panic::set_hook(Box::new(move |info| sink(&format!("panic: {info}").replace('\n', " "))));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn ok(args: &[&str]) -> Opts {
        parse(args).unwrap()
    }

    #[test]
    fn defaults() {
        let o = ok(&["work"]);
        assert_eq!((o.alias.as_str(), o.hotkey.as_str(), o.apps.as_slice()), ("work", "Ctrl+Alt+V", &["rio.exe".to_string()][..]));
        assert_eq!((o.mods, o.vk), (MOD_CONTROL | MOD_ALT, 'V' as u32));
    }

    #[test]
    fn hotkey_forms() {
        for (given, text, mods, vk) in [
            ("win+shift+f24", "Shift+Win+F24", MOD_SHIFT | MOD_WIN, 0x87),
            ("Alt+F1", "Alt+F1", MOD_ALT, 0x70),
            ("CTRL+7", "Ctrl+7", MOD_CONTROL, '7' as u32),
            ("ctrl+alt+v", "Ctrl+Alt+V", MOD_CONTROL | MOD_ALT, 'V' as u32),
        ] {
            let o = ok(&["a", "--hotkey", given]);
            assert_eq!((o.hotkey.as_str(), o.mods, o.vk), (text, mods, vk), "{given}");
        }
    }

    #[test]
    fn bad_hotkeys_say_which_part() {
        for bad in ["", "Ctrl+", "Ctrl+Alt", "Ctrl+F0", "Ctrl+F25", "Ctrl+F01", "Ctrl+AB", "Ctrl+Ctrl+V", "Ctrl+Meta+V", "Ctrl++V", "Ctrl+-", "Ctrl+É"] {
            let e = parse(&["a", "--hotkey", bad]).unwrap_err();
            assert!(e.starts_with("bad --hotkey"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn apps_are_lower_case_and_trimmed() {
        assert_eq!(ok(&["a", "--apps", "Rio.EXE, Code.exe"]).apps, ["rio.exe", "code.exe"]);
    }

    #[test]
    fn bad_apps_say_which_name() {
        for bad in ["", "rio", "rio.exe,", ",rio.exe", "a\\b.exe", ".exe", "a|b.exe"] {
            let e = parse(&["a", "--apps", bad]).unwrap_err();
            assert!(e.starts_with("bad --apps"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn bad_command_lines() {
        for bad in [&[][..], &["--hotkey", "F5"], &[""], &["a", "--hotkey"], &["a", "--apps"], &["a", "--colour", "red"], &["a", "b"]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn run_gets_what_start_checked() {
        let o = ok(&["work", "--apps", "Rio.exe,wt.exe", "--hotkey", "shift+win+F5"]);
        let args = o.run_args();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(args[0], "run");
        assert_eq!(ok(&args[1..]), o);
    }

    #[test]
    fn state_round_trip() {
        let s = State { pid: 4321, start: 133_800_000_000_000_000, version: "1.0.0".into() };
        let t = State::parse(&format!("{}channel: connected\n", s.text())).unwrap();
        assert_eq!((t.pid, t.start, t.version.as_str()), (4321, 133_800_000_000_000_000, "1.0.0"));
        for bad in ["", "12", "12\t13", "x\t1\t1.0.0", "1\t-1\t1.0.0"] {
            assert!(State::parse(bad).is_none(), "{bad:?}");
        }
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ferry-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn log_rotates_past_one_mib_and_keeps_one_old_copy() {
        let d = temp("rotate");
        let log = d.join("x.log");
        let old = d.join("x.log.1");
        fs::write(&log, vec![b'a'; LOG_MAX as usize]).unwrap();
        log_line(&log, "first");
        assert!(!old.exists(), "a file of exactly 1 MiB stays");
        log_line(&log, "second");
        assert_eq!(fs::metadata(&old).unwrap().len() as usize, LOG_MAX as usize + "YYYY-MM-DD HH:MM:SS first\n".len());
        let now = fs::read_to_string(&log).unwrap();
        assert!(now.ends_with(" second\n") && now.lines().count() == 1, "{now:?}");
        fs::write(&log, vec![b'b'; LOG_MAX as usize + 1]).unwrap();
        rotate(&log);
        assert_eq!(fs::read(&old).unwrap()[0], b'b', "the new old copy replaces the older one");
        assert!(!log.exists());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn log_errors_do_not_panic() {
        let nowhere = std::env::temp_dir().join(format!("ferry-test-{}-missing", std::process::id())).join("x.log");
        log_line(&nowhere, "lost");
        rotate(&nowhere);
        assert!(!nowhere.exists());
    }

    #[test]
    fn panic_hook_gives_message_and_location() {
        let seen = Arc::new(Mutex::new(String::new()));
        let s2 = seen.clone();
        panic_hook(move |m| *s2.lock().unwrap() = m.to_string());
        let _ = std::panic::catch_unwind(|| panic!("boom {}", 7));
        let _ = std::panic::take_hook();
        let m = seen.lock().unwrap().clone();
        assert!(m.contains("boom 7") && m.contains("lifecycle.rs:") && !m.contains('\n'), "{m:?}");
    }
}
