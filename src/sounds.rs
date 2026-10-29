// Sounds of the Workstation. The exe holds the WAV files and plays them from
// memory. One quiet window covers all sounds: a sound plays only if no sound
// played in the last `QUIET`, or if its rank is higher than the rank of the
// last sound that played. A dropped sound does not start a new window.

use std::sync::Mutex;
use std::time::{Duration, Instant};

const QUIET: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sound {
    Blocked,
    Done,
    Arrival,
    Error,
    Clip,
}

impl Sound {
    fn rank(self) -> u8 {
        match self {
            Sound::Done | Sound::Arrival => 1,
            Sound::Blocked => 2,
            Sound::Error | Sound::Clip => 3,
        }
    }

    fn wav(self) -> &'static [u8] {
        match self {
            Sound::Blocked => include_bytes!("../sounds/blocked-a.wav"),
            Sound::Done => include_bytes!("../sounds/done-a.wav"),
            Sound::Arrival => include_bytes!("../sounds/arrival-b.wav"),
            Sound::Error => include_bytes!("../sounds/error-a.wav"),
            Sound::Clip => include_bytes!("../sounds/clip-b.wav"),
        }
    }
}

// The last sound that played: when, and its rank.
struct Window(Option<(Instant, u8)>);

impl Window {
    fn admit(&mut self, now: Instant, rank: u8) -> bool {
        let play = self.0.is_none_or(|(at, last)| now.duration_since(at) >= QUIET || rank > last);
        if play {
            self.0 = Some((now, rank));
        }
        play
    }
}

static WINDOW: Mutex<Window> = Mutex::new(Window(None));

// Every sound that `play` was asked for, also those that the quiet window drops.
#[cfg(test)]
pub static ASKED: Mutex<Vec<Sound>> = Mutex::new(Vec::new());

// Plays `s` when the quiet window allows it. A new sound stops the one that plays.
pub fn play(s: Sound) {
    #[cfg(test)]
    ASKED.lock().unwrap_or_else(|e| e.into_inner()).push(s);
    let admitted = WINDOW.lock().unwrap_or_else(|e| e.into_inner()).admit(Instant::now(), s.rank());
    #[cfg(windows)]
    if admitted {
        const SND_ASYNC: u32 = 0x1;
        const SND_NODEFAULT: u32 = 0x2;
        const SND_MEMORY: u32 = 0x4;
        #[link(name = "winmm")]
        extern "system" {
            fn PlaySoundW(sound: *const u8, hmod: *mut std::ffi::c_void, flags: u32) -> i32;
        }
        // The WAV is `'static`, so it stays valid while the async sound plays.
        unsafe { PlaySoundW(s.wav().as_ptr(), std::ptr::null_mut(), SND_MEMORY | SND_ASYNC | SND_NODEFAULT) };
    }
    #[cfg(not(windows))]
    let _ = (admitted, s.wav());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    #[test]
    fn first_sound_plays() {
        assert!(Window(None).admit(Instant::now(), 1));
    }

    #[test]
    fn equal_or_lower_rank_inside_window_is_dropped() {
        let t0 = Instant::now();
        let mut w = Window(None);
        assert!(w.admit(t0, 2));
        assert!(!w.admit(ms(t0, 100), 2));
        assert!(!w.admit(ms(t0, 100), 1));
    }

    #[test]
    fn higher_rank_inside_window_plays() {
        let t0 = Instant::now();
        let mut w = Window(None);
        assert!(w.admit(t0, 1));
        assert!(w.admit(ms(t0, 100), 3));
        // The error sound is now the last one: Blocked no longer beats it.
        assert!(!w.admit(ms(t0, 200), 2));
    }

    #[test]
    fn any_sound_plays_after_the_window() {
        let t0 = Instant::now();
        let mut w = Window(None);
        assert!(w.admit(t0, 3));
        assert!(!w.admit(ms(t0, 1499), 1));
        assert!(w.admit(ms(t0, 1500), 1));
    }

    #[test]
    fn dropped_sound_does_not_start_a_window() {
        let t0 = Instant::now();
        let mut w = Window(None);
        assert!(w.admit(t0, 1));
        assert!(!w.admit(ms(t0, 1000), 1));
        // 1.5 s after the sound that played, not after the dropped one.
        assert!(w.admit(ms(t0, 1600), 1));
    }
}
