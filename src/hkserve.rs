// Remote side of the hotkey flow. The Workstation starts it through a hidden
// `ssh.exe -T`. It reads files from stdin, saves them under
// ~/.local/state/ferry/in/, and types their paths into the herdr pane that
// `pane.current` gives (`pane.send_input`, bracketed paste, no Enter).
//
// Protocol, one line per reply:
//   in:  "SEND <n>\n", then n times "<len> <name>\n" + <len> bytes
//   out: "PASTED\t<pane>\t<agent>\t<ms>\t<paths>\n"
//        "CLIP\t<reason>\t<ms>\t<paths>\n"   (the Workstation puts the paths on the clipboard)

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn run() -> i32 {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let land = crate::remote::state().join("in");
    let sock = home.join(".config/herdr/herdr.sock");
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut out = std::io::stdout().lock();
    eprintln!("hk-serve: ready, pid {}", std::process::id());

    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).unwrap_or(0) == 0 {
            eprintln!("hk-serve: EOF, exit");
            return 0;
        }
        let n: usize = line.trim().strip_prefix("SEND ").and_then(|s| s.parse().ok()).expect("bad SEND line");
        let t = Instant::now();
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
        let dir = land.join(stamp.to_string());
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).unwrap();

        let mut paths = Vec::new();
        for _ in 0..n {
            line.clear();
            input.read_line(&mut line).unwrap();
            let (len, name) = line.trim_end_matches('\n').split_once(' ').expect("bad file header");
            let mut data = vec![0u8; len.parse().unwrap()];
            input.read_exact(&mut data).unwrap();
            let path = dir.join(sanitize(name));
            std::fs::write(&path, &data).unwrap();
            paths.push(path.display().to_string());
        }
        let text = paths.join(" ");
        let reply = match paste(&sock, &text) {
            Ok((pane, agent)) => format!("PASTED\t{pane}\t{agent}\t{}\t{text}", t.elapsed().as_millis()),
            Err(e) => format!("CLIP\t{e}\t{}\t{text}", t.elapsed().as_millis()),
        };
        eprintln!("hk-serve: {reply}");
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
    }
}

// Keep [A-Za-z0-9._-] so that a path needs no quotes and no JSON escapes.
fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect()
}

fn paste(sock: &Path, text: &str) -> Result<(String, String), String> {
    let cur = call(sock, r#"{"id":"1","method":"pane.current","params":{}}"#)?;
    let pane = field(&cur, "pane_id").ok_or_else(|| format!("pane.current: {}", cur.trim()))?;
    let agent = field(&cur, "agent").unwrap_or_else(|| "none".into());
    let req = format!(r#"{{"id":"2","method":"pane.send_input","params":{{"pane_id":"{pane}","text":"{text}"}}}}"#);
    let r = call(sock, &req)?;
    if r.contains(r#""error""#) {
        return Err(format!("pane.send_input: {}", r.trim()));
    }
    Ok((pane, agent))
}

fn call(sock: &Path, req: &str) -> Result<String, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| format!("herdr socket: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    writeln!(s, "{req}").map_err(|e| e.to_string())?;
    let mut r = String::new();
    BufReader::new(s).read_line(&mut r).map_err(|e| e.to_string())?;
    Ok(r)
}

// First `"key":"value"` in the reply. Enough for the herdr replies used here.
fn field(json: &str, key: &str) -> Option<String> {
    let start = json.find(&format!(r#""{key}":""#))? + key.len() + 4;
    Some(json[start..].split('"').next()?.to_string())
}
