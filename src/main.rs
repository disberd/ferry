// ferry: one binary for both sides of the Exchange folder and screenshot paths.
//   Remote (Linux):      serve | ls | pull | push | hk-serve | hook
//   Workstation (Windows): start | run | stop | status
// Both: --version | version

// The Workstation core runs on Linux too, but only the tests use it there.
#![cfg_attr(not(any(windows, test)), allow(dead_code))]

mod frame;
#[cfg(unix)]
mod hkserve;
#[cfg(unix)]
mod remote;
mod exchange;
#[cfg(windows)]
mod hotkey;
#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod windows;
#[cfg(all(test, unix))]
mod loopback;

const USAGE: &str = "usage: ferry --version | version | serve | ls [dir] | pull <xpath> [local|-] | push <local> [xpath] | hk-serve | hook | start <alias> | run <alias> | stop | status";

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
        ["hk-serve"] => hkserve::run(),
        #[cfg(unix)]
        ["hook"] => {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            0
        }
        #[cfg(windows)]
        ["start", alias] => windows::start(alias),
        #[cfg(windows)]
        ["run", alias] => windows::run(alias),
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
