//! The Pointerses resident daemon.
//!
//! The first `pss run` of a program spawns a long-lived daemon process
//! (`pss daemon`) that compiles the program once and keeps the memory snapshot
//! resident. Subsequent `pss run` invocations connect to the daemon over a
//! loopback TCP socket and receive the compiled snapshot, so they start from
//! compiled bytecode instead of re-running the whole front-end (target
//! < 200 ms startup).
//!
//! Wire protocol (line-oriented on top of TCP):
//!   client -> `GET <path>\n`
//!   server -> `OK <len>\n<bytes>`   |   `MISS\n`
//!   client -> `PUT <path> <len>\n<bytes>`
//!   server -> `OK\n`

pub mod snapshot;
pub use snapshot::{build_snapshot, snapshot_json};

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

/// Default loopback port used by the daemon.
pub const DEFAULT_PORT: u16 = 40123;

const HOST: &str = "127.0.0.1";

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Run the daemon server (blocks forever).
pub fn serve(port: u16) -> Result<u8, String> {
    let listener = TcpListener::bind((HOST, port))
        .map_err(|e| format!("cannot bind daemon on {HOST}:{port}: {e}"))?;
    let cache: std::sync::Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    eprintln!("pointerses daemon listening on {HOST}:{port}");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let cache = cache.clone();
                std::thread::spawn(move || handle(s, cache));
            }
            Err(_) => break,
        }
    }
    Ok(0)
}

fn handle(stream: TcpStream, cache: std::sync::Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>>) {
    let mut write = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let _ = write.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).is_err() {
        return;
    }
    let first_line = first_line.trim().to_string();
    if let Some(rest) = first_line.strip_prefix("GET ") {
        let path = rest.to_string();
        let hit = { cache.lock().unwrap().get(&path).cloned() };
        match hit {
            Some(bytes) => {
                let header = format!("OK {}\n", bytes.len());
                let _ = write.write_all(header.as_bytes());
                let _ = write.write_all(&bytes);
            }
            None => {
                let _ = write.write_all(b"MISS\n");
            }
        }
        let _ = write.flush();
    } else if let Some(rest) = first_line.strip_prefix("PUT ") {
        // PUT <path> <len>
        let (path, len) = match rest.split_once(' ') {
            Some((p, l)) => (p.to_string(), l.parse::<usize>().unwrap_or(0)),
            None => return,
        };
        let mut buf = vec![0u8; len];
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        cache.lock().unwrap().insert(path, buf);
        let _ = write.write_all(b"OK\n");
        let _ = write.flush();
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Try to obtain a compiled snapshot for `file` from a running daemon.
/// Returns the bytecode bytes if cached, else `None`.
pub fn try_fast_start(file: &str) -> Option<Vec<u8>> {
    let mut stream = TcpStream::connect((HOST, DEFAULT_PORT)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let path = absolute(file);
    let _ = stream.write_all(format!("GET {path}\n").as_bytes());
    let _ = stream.flush();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return None;
    }
    let line = line.trim().to_string();
    if let Some(rest) = line.strip_prefix("OK ") {
        let len = rest.parse::<usize>().ok()?;
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).ok()?;
        Some(buf)
    } else {
        None
    }
}

/// Warm the daemon cache with freshly compiled bytecode for `file`.
pub fn warm_cache(file: &str, bytes: Vec<u8>) {
    // Ensure a daemon is running.
    if TcpStream::connect((HOST, DEFAULT_PORT)).is_err() {
        spawn_daemon();
        // give it a moment to bind
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
    if let Ok(mut stream) = TcpStream::connect((HOST, DEFAULT_PORT)) {
        let path = absolute(file);
        let _ = stream.write_all(format!("PUT {path} {}\n", bytes.len()).as_bytes());
        let _ = stream.write_all(&bytes);
        let _ = stream.flush();
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
        let mut line = String::new();
        let _ = BufReader::new(&mut stream).read_line(&mut line);
    }
}

/// Spawn the daemon as a detached background process.
fn spawn_daemon() {
    if let Ok(exe) = std::env::current_exe() {
        let mut child = std::process::Command::new(exe);
        child.arg("daemon");
        // detach from our stdio so it can outlive the caller
        use std::process::Stdio;
        child.stdout(Stdio::null()).stderr(Stdio::null()).stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW (0x08000000) | DETACHED_PROCESS (0x00000008)
            const DETACHED: u32 = 0x08000008;
            child.creation_flags(DETACHED);
        }
        let _ = child.spawn();
    }
}

fn absolute(file: &str) -> String {
    std::fs::canonicalize(file)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| file.to_string())
}
