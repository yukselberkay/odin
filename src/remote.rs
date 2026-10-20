//! `odin FILE` typed in one of Odin's own terminal panes opens FILE in that Odin
//! instead of starting another one inside it. Odin listens on a socket in a private
//! directory and tells its terminals where (`ODIN_SOCKET`); the inner `odin` sends
//! the path there and exits. Unix only: elsewhere `odin` starts as usual.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender},
        OnceLock,
    },
    time::Duration,
};

pub const VAR: &str = "ODIN_SOCKET";

/// The socket of this Odin, handed to every terminal pane it starts.
static ADDRESS: OnceLock<PathBuf> = OnceLock::new();

pub fn address() -> Option<&'static Path> {
    ADDRESS.get().map(PathBuf::as_path)
}

#[derive(Serialize, Deserialize)]
struct Ask {
    open: PathBuf,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    /// Done; the text says what happened.
    Opened(String),
    /// Not for this Odin (another project): start a separate one.
    Elsewhere,
    Failed(String),
}

/// One `odin PATH` from a terminal pane, answered from the main loop.
pub struct Request {
    pub path: PathBuf,
    reply: Sender<Reply>,
}

impl Request {
    pub fn answer(self, reply: Reply) {
        let _ = self.reply.send(reply);
    }
}

pub struct Server {
    pub requests: Receiver<Request>,
    dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
pub fn listen() -> Result<Server> {
    use std::{io::BufRead, io::Write, os::unix::fs::DirBuilderExt, os::unix::net::UnixListener};
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!("odin-{}-{stamp:x}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("Create {}", dir.display()))?;
    let socket = dir.join("socket");
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error).context("Listen for `odin FILE` from terminal panes");
        }
    };
    let (sender, requests) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let sender: Sender<Request> = sender.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut line = String::new();
                let reader = std::io::BufReader::new(&stream);
                if std::io::Read::take(reader, 64 * 1024)
                    .read_line(&mut line)
                    .is_err()
                {
                    return;
                }
                let reply = match serde_json::from_str::<Ask>(&line) {
                    Ok(ask) => {
                        let (reply, answer) = mpsc::channel();
                        let request = Request {
                            path: ask.open,
                            reply,
                        };
                        if sender.send(request).is_err() {
                            return;
                        }
                        answer
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap_or_else(|_| Reply::Failed("Odin is busy; try again".into()))
                    }
                    Err(_) => Reply::Failed("Unreadable request".into()),
                };
                if let Ok(json) = serde_json::to_string(&reply) {
                    let _ = writeln!(stream, "{json}");
                }
            });
        }
    });
    let _ = ADDRESS.set(socket);
    Ok(Server { requests, dir })
}

#[cfg(not(unix))]
pub fn listen() -> Result<Server> {
    bail!("Opening files from terminal panes needs Unix sockets")
}

/// Hand PATH (absolute) to the Odin at SOCKET. An error means no Odin answered, so
/// the caller should start normally.
#[cfg(unix)]
pub fn send(socket: &Path, path: &Path) -> Result<Reply> {
    use std::{io::BufRead, io::Write, os::unix::net::UnixStream};
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let json = serde_json::to_string(&Ask {
        open: path.to_path_buf(),
    })?;
    writeln!(stream, "{json}")?;
    let mut line = String::new();
    std::io::BufReader::new(&stream).read_line(&mut line)?;
    if line.is_empty() {
        bail!("Odin closed the connection");
    }
    Ok(serde_json::from_str(&line)?)
}

#[cfg(not(unix))]
pub fn send(_socket: &Path, _path: &Path) -> Result<Reply> {
    bail!("Opening files from terminal panes needs Unix sockets")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_hands_its_path_to_the_running_odin() {
        let server = listen().unwrap();
        let socket = address().unwrap().to_path_buf();
        let dir = socket.parent().unwrap().to_path_buf();
        let client = std::thread::spawn({
            let socket = socket.clone();
            move || send(&socket, Path::new("/project/notes.txt")).unwrap()
        });
        let request = server
            .requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(request.path, Path::new("/project/notes.txt"));
        request.answer(Reply::Opened("Opened notes.txt".into()));
        assert_eq!(
            client.join().unwrap(),
            Reply::Opened("Opened notes.txt".into())
        );
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "only you can reach the socket");
        }
        drop(server);
        assert!(!dir.exists(), "the socket goes away with Odin");
        assert!(
            send(&socket, Path::new("/x")).is_err(),
            "then odin starts normally"
        );
    }
}
