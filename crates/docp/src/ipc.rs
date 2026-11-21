//! Line-delimited JSON over a Unix socket between the preview and an editor.
//!
//! The preview is the server: it creates the socket, accepts one client at a
//! time and never blocks the UI thread on socket I/O. A dedicated thread reads
//! the socket and forwards events over a channel; writes go through a small
//! mutex-guarded handle and are best-effort.
//!
//! Editor -> preview: `{"v":1,"type":"sync","line":42}`, `{"v":1,"type":"close"}`
//! Preview -> editor: `{"v":1,"type":"ready"}`, `{"v":1,"type":"top","line":42,"block":7}`
//! (`line` is 1-based; `block` is 1-based and informational only.)

use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ACCEPT_POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// An editor connected; the preview answers with `ready`.
    Connected,
    Sync {
        line: usize,
    },
    /// The editor is going away; drop the client but keep the preview.
    Close,
    Disconnected,
}

#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    #[allow(dead_code)]
    v: u32,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    line: Option<usize>,
}

fn parse(line: &str) -> Option<Event> {
    let w: Wire = serde_json::from_str(line).ok()?;
    match w.kind.as_str() {
        "sync" => Some(Event::Sync { line: w.line? }),
        "close" => Some(Event::Close),
        _ => None,
    }
}

pub struct Ipc {
    path: PathBuf,
    client: Arc<Mutex<Option<UnixStream>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Ipc {
    /// Bind `path` (replacing a stale socket) and start the reader thread.
    pub fn listen(path: &Path) -> Result<(Self, Receiver<Event>)> {
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
        }
        let listener = UnixListener::bind(path)
            .with_context(|| format!("binding IPC socket {}", path.display()))?;
        listener.set_nonblocking(true)?;
        let client = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = unbounded();
        let thread = {
            let (client, stop, path) = (client.clone(), stop.clone(), path.to_path_buf());
            std::thread::Builder::new()
                .name("docp-ipc".into())
                .spawn(move || serve(listener, client, stop, tx, path))
                .context("spawning IPC thread")?
        };
        Ok((
            Self {
                path: path.to_path_buf(),
                client,
                stop,
                thread: Some(thread),
            },
            rx,
        ))
    }

    pub fn send_ready(&self) {
        self.write("{\"v\":1,\"type\":\"ready\"}\n");
    }

    /// `line` and `block` are 1-based.
    pub fn send_top(&self, line: usize, block: usize) {
        self.write(&format!(
            "{{\"v\":1,\"type\":\"top\",\"line\":{line},\"block\":{block}}}\n"
        ));
    }

    /// Close the current client (the plugin may reconnect later).
    pub fn disconnect(&self) {
        let stream = self.client.lock().map(|mut c| c.take()).ok().flatten();
        if let Some(s) = stream {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    fn write(&self, msg: &str) {
        let Ok(mut client) = self.client.lock() else {
            return;
        };
        let Some(stream) = client.as_mut() else {
            return;
        };
        if stream.write_all(msg.as_bytes()).is_err() {
            *client = None;
        }
    }
}

impl Drop for Ipc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.disconnect();
        // Do not join: the reader may be parked in a blocking read on a client
        // that is still connected. A detached thread exits with the process.
        let _ = self.thread.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn serve(
    listener: UnixListener,
    client: Arc<Mutex<Option<UnixStream>>>,
    stop: Arc<AtomicBool>,
    tx: Sender<Event>,
    path: PathBuf,
) {
    while !stop.load(Ordering::Relaxed) {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(_) => break,
        };
        let _ = stream.set_nonblocking(false);
        if tx.send(Event::Connected).is_err() {
            break;
        }
        match stream.try_clone() {
            Ok(w) => {
                let _ = w.set_nonblocking(false);
                if let Ok(mut c) = client.lock() {
                    *c = Some(w);
                }
            }
            Err(_) => {
                let _ = tx.send(Event::Disconnected);
                continue;
            }
        }
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            match parse(&line) {
                Some(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                None => continue,
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
        if let Ok(mut c) = client.lock() {
            *c = None;
        }
        let _ = tx.send(Event::Disconnected);
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_messages_only() {
        assert_eq!(
            parse("{\"v\":1,\"type\":\"sync\",\"line\":42}"),
            Some(Event::Sync { line: 42 })
        );
        assert_eq!(parse("{\"v\":1,\"type\":\"close\"}"), Some(Event::Close));
        assert_eq!(parse("{\"type\":\"sync\"}"), None);
        assert_eq!(parse("{\"type\":\"nope\"}"), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn socket_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("docp-ipc-test-{}.sock", std::process::id()));
        let (_ipc, rx) = Ipc::listen(&path).unwrap();

        let mut client = UnixStream::connect(&path).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(Event::Connected)
        );

        client
            .write_all(b"{\"v\":1,\"type\":\"sync\",\"line\":7}\n")
            .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(Event::Sync { line: 7 })
        );

        client.write_all(b"{\"v\":1,\"type\":\"close\"}\n").unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Event::Close));
        drop(client);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(Event::Disconnected)
        );
        drop(_ipc);
        assert!(!path.exists());
    }
}
