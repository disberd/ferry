// Stand-ins for the Win32 calls of the Workstation core, so that it builds on
// Linux. The tests read the log through the test output.

pub fn log(msg: &str) {
    eprintln!("{msg}");
}
