// When the hotkey is active. It holds the decisions that need no Win32 code: which exe
// names are on the list, when the key must be registered or released, when a failed
// registration plays the error sound, and the text of the `hotkey:` status line.
// `hotkey.rs` feeds it the foreground exe and does the Win32 calls.

// The exe is on the list of `--apps`. The case does not matter.
pub fn listed(apps: &[String], exe: &str) -> bool {
    apps.iter().any(|a| a.eq_ignore_ascii_case(exe))
}

#[derive(Debug, PartialEq)]
pub enum Act {
    Register,
    Unregister,
    Keep,
}

// What a registration attempt asks of the caller.
#[derive(Debug, PartialEq)]
pub struct Tried {
    // Play the error sound: the first failure in this process run.
    pub sound: bool,
    // Write the log line: the key was free or untried before this failure.
    pub log: bool,
}

pub struct Active {
    apps: Vec<String>,
    on: bool,
    taken: bool,
    warned: bool,
}

impl Active {
    pub fn new(apps: &[String]) -> Active {
        Active { apps: apps.to_vec(), on: false, taken: false, warned: false }
    }

    // The foreground exe is `exe`. A failed attempt leaves the key off, so each focus on an app
    // of the list tries again.
    pub fn focus(&self, exe: &str) -> Act {
        match (self.listed(exe), self.on) {
            (true, false) => Act::Register,
            (false, true) => Act::Unregister,
            _ => Act::Keep,
        }
    }

    pub fn listed(&self, exe: &str) -> bool {
        listed(&self.apps, exe)
    }

    // The result of `RegisterHotKey`. A key that stays taken gives no new log line, so a
    // user who switches between windows does not fill the log.
    pub fn registered(&mut self, ok: bool) -> Tried {
        self.on = ok;
        let tried = Tried { sound: !ok && !self.warned, log: !ok && !self.taken };
        self.warned |= !ok;
        self.taken = !ok;
        tried
    }

    pub fn unregistered(&mut self) {
        self.on = false;
    }

    // Another program holds the key, as of the last attempt.
    pub fn taken(&self) -> bool {
        self.taken
    }
}

// What the `hotkey:` line shows besides the key and the apps.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    // The key that another program holds.
    pub taken: Option<String>,
    // The foreground exe, and if it is on the list. `None` before the first check.
    pub exe: Option<(String, bool)>,
}

impl Seen {
    // The clash text, for the status line and the tray.
    pub fn clash(&self) -> Option<String> {
        self.taken.as_ref().map(|k| format!("hotkey {k}: taken by another program"))
    }

    // The text after `hotkey: `. `base` is `<key> for <apps>`.
    pub fn line(&self, base: &str) -> String {
        let head = self.clash().unwrap_or_else(|| base.into());
        match &self.exe {
            None => head,
            Some((e, on)) => format!("{head} · foreground now: {e} ({})", if *on { "on the list" } else { "not on the list" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active() -> Active {
        Active::new(&["rio.exe".into(), "wezterm-gui.exe".into()])
    }

    #[test]
    fn the_list_ignores_the_case_and_wants_the_whole_name() {
        let a = ["rio.exe".to_string()];
        assert!(listed(&a, "Rio.EXE"));
        assert!(!listed(&a, "rio.exe.bak"));
        assert!(!listed(&a, "audio.exe"));
        assert!(!listed(&a, ""));
    }

    #[test]
    fn the_key_follows_the_focus() {
        let mut a = active();
        assert_eq!(a.focus("chrome.exe"), Act::Keep);
        assert_eq!(a.focus("RIO.exe"), Act::Register);
        a.registered(true);
        assert_eq!(a.focus("rio.exe"), Act::Keep);
        assert_eq!(a.focus("wezterm-gui.exe"), Act::Keep);
        assert_eq!(a.focus("chrome.exe"), Act::Unregister);
        a.unregistered();
        assert_eq!(a.focus("chrome.exe"), Act::Keep);
        assert_eq!(a.focus("rio.exe"), Act::Register);
    }

    #[test]
    fn a_taken_key_is_tried_again_at_each_focus_on_the_list() {
        let mut a = active();
        assert_eq!(a.focus("rio.exe"), Act::Register);
        a.registered(false);
        assert!(a.taken());
        assert_eq!(a.focus("rio.exe"), Act::Register);
        assert_eq!(a.focus("chrome.exe"), Act::Keep);
        a.registered(true);
        assert!(!a.taken());
    }

    #[test]
    fn the_error_sound_plays_once_in_a_run_and_the_log_line_once_for_each_clash() {
        let mut a = active();
        assert_eq!(a.registered(false), Tried { sound: true, log: true });
        assert_eq!(a.registered(false), Tried { sound: false, log: false });
        assert_eq!(a.registered(true), Tried { sound: false, log: false });
        assert_eq!(a.registered(false), Tried { sound: false, log: true });
    }

    #[test]
    fn the_status_line_has_the_forms_of_the_spec() {
        let base = "Ctrl+Alt+V for rio.exe";
        let mut s = Seen::default();
        assert_eq!(s.line(base), base);
        s.exe = Some(("rio.exe".into(), true));
        assert_eq!(s.line(base), "Ctrl+Alt+V for rio.exe · foreground now: rio.exe (on the list)");
        s.exe = Some(("chrome.exe".into(), false));
        assert_eq!(s.line(base), "Ctrl+Alt+V for rio.exe · foreground now: chrome.exe (not on the list)");
        s.taken = Some("Ctrl+Alt+V".into());
        assert_eq!(s.line(base), "hotkey Ctrl+Alt+V: taken by another program · foreground now: chrome.exe (not on the list)");
        assert_eq!(s.clash().as_deref(), Some("hotkey Ctrl+Alt+V: taken by another program"));
    }
}
