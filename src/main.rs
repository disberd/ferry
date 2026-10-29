// ferry: one binary for both sides of the Exchange folder and screenshot paths.
//   Remote (Linux):      serve | ls | pull | push | hook
//   Workstation (Windows): start | run | stop | status
// Both: --version | version

// The Workstation core runs on Linux too, but only the tests use it there.
#![cfg_attr(not(any(windows, test)), allow(dead_code))]

mod frame;
mod lifecycle;
mod link;
#[cfg_attr(not(windows), allow(dead_code))]
mod sounds;
#[cfg(unix)]
mod remote;
mod exchange;
#[cfg(windows)]
mod hotkey;
#[cfg(windows)]
mod tray;
#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod windows;
#[cfg(all(test, unix))]
mod loopback;

const USAGE: &str = "usage: ferry --version | version | serve | ls [dir] | pull <xpath> [local|-] | push <local> [xpath] | hook | start <alias> [--hotkey <key>] [--apps <exe>,<exe>] | run <alias> [--hotkey <key>] [--apps <exe>,<exe>] | stop | status";

// Checks the options of `start` and `run` before anything starts.
#[cfg(windows)]
fn with_opts(args: &[&str], f: fn(&lifecycle::Opts) -> i32) -> i32 {
    match lifecycle::parse(args) {
        Ok(o) => f(&o),
        Err(e) => {
            eprintln!("ferry: {e}");
            2
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let code = match a[..] {
        ["--version"] | ["version"] => {
            println!("ferry {}", frame::VERSION);
            0
        }
        #[cfg(unix)]
        ["serve"] => remote::serve(),
        #[cfg(unix)]
        ["ls"] => remote::ls(""),
        #[cfg(unix)]
        ["ls", d] => remote::ls(d),
        #[cfg(unix)]
        ["pull", x, ref dest @ ..] if dest.len() <= 1 => remote::pull(x, dest.first().copied()),
        #[cfg(unix)]
        ["push", l] => remote::push(l, remote::base(l)),
        #[cfg(unix)]
        ["push", l, x] => remote::push(l, x),
        #[cfg(unix)]
        ["hook"] => {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            0
        }
        #[cfg(windows)]
        ["start", ref r @ ..] => with_opts(r, windows::start),
        #[cfg(windows)]
        ["run", ref r @ ..] => with_opts(r, windows::run),
        #[cfg(windows)]
        ["stop"] => windows::stop(),
        #[cfg(windows)]
        ["status"] => windows::status(),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
