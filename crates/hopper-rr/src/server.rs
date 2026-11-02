//! A debug-server child process (gdbserver or `rr replay -s`): start it, find the port it
//! listens on from its stderr, keep draining stderr so it never blocks, and kill it on drop.

use std::collections::VecDeque;
use std::fmt;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hopper_core::{HopperError, Result};

/// Lines of stderr kept for error messages.
const KEPT_LINES: usize = 200;

/// Where a server listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenAddr {
    pub host: String,
    pub port: u16,
}

/// A running server process.
pub struct ServerProcess {
    child: Child,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    description: String,
}

impl fmt::Debug for ServerProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerProcess")
            .field("pid", &self.child.id())
            .field("command", &self.description)
            .finish()
    }
}

impl ServerProcess {
    /// Spawn `cmd` with stderr piped, and read stderr lines until `find_addr` recognises the
    /// listening address, the process exits, or `timeout` passes.
    pub fn spawn(
        mut cmd: Command,
        find_addr: fn(&str) -> Option<ListenAddr>,
        timeout: Duration,
    ) -> Result<(ServerProcess, ListenAddr)> {
        let description = format!("{cmd:?}");
        cmd.stdin(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| HopperError::Io(format!("cannot start {description}: {e}")))?;
        let stderr = child.stderr.take().expect("stderr is piped");
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, rx) = mpsc::channel::<String>();
        let tail_w = tail.clone();
        std::thread::Builder::new()
            .name("hopper-server-stderr".into())
            .spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let l = line.trim_end().to_string();
                            if let Ok(mut t) = tail_w.lock() {
                                if t.len() == KEPT_LINES {
                                    t.pop_front();
                                }
                                t.push_back(l.clone());
                            }
                            // The receiver goes away once the port is found; keep draining.
                            let _ = tx.send(l);
                        }
                    }
                }
            })
            .map_err(|e| HopperError::Io(format!("cannot start stderr reader: {e}")))?;

        let mut server = ServerProcess {
            child,
            stderr_tail: tail,
            description,
        };
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    if let Some(addr) = find_addr(&line) {
                        return Ok((server, addr));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(HopperError::Io(format!(
                        "{} printed no listening port within {timeout:?}; stderr:\n{}",
                        server.description,
                        server.stderr_text()
                    )));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = server.child.wait().ok();
                    return Err(HopperError::Disconnected(format!(
                        "{} exited ({status:?}) before listening; stderr:\n{}",
                        server.description,
                        server.stderr_text()
                    )));
                }
            }
        }
    }

    /// Connect to the server, retrying briefly while it finishes binding.
    pub fn connect(&mut self, addr: &ListenAddr) -> Result<TcpStream> {
        let mut last = None;
        for _ in 0..50 {
            match TcpStream::connect((addr.host.as_str(), addr.port)) {
                Ok(s) => {
                    s.set_nodelay(true)?;
                    return Ok(s);
                }
                Err(e) => last = Some(e),
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(HopperError::Disconnected(format!(
                    "{} exited ({status}); stderr:\n{}",
                    self.description,
                    self.stderr_text()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(HopperError::Io(format!(
            "cannot connect to {}:{}: {last:?}",
            addr.host, addr.port
        )))
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The last lines the server wrote to stderr.
    pub fn stderr_text(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// gdbserver prints `Listening on port 33785` (to stderr).
pub fn gdbserver_listen_addr(line: &str) -> Option<ListenAddr> {
    let port = line
        .trim()
        .strip_prefix("Listening on port ")?
        .trim()
        .parse()
        .ok()?;
    Some(ListenAddr {
        host: "127.0.0.1".into(),
        port,
    })
}

/// `rr replay -s` prints `Launch debugger with` and then the gdb command line, each argument in
/// single quotes, one of them `'target extended-remote <host>:<port>'` (or
/// `tcp6:[<host>]:<port>`) (`launch_debugger.cc`, `push_gdb_target_remote_cmd`).
pub fn rr_listen_addr(line: &str) -> Option<ListenAddr> {
    const KEY: &str = "target extended-remote ";
    let start = line.find(KEY)? + KEY.len();
    let rest = &line[start..];
    let end = rest.find(['\'', ' ']).unwrap_or(rest.len());
    let target = &rest[..end];
    let (host, port) = if let Some(v6) = target.strip_prefix("tcp6:[") {
        let (h, p) = v6.split_once("]:")?;
        (h.to_string(), p)
    } else {
        let (h, p) = target.rsplit_once(':')?;
        (h.to_string(), p)
    };
    let host = if host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        host
    };
    Some(ListenAddr {
        host,
        port: port.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_gdbserver_port() {
        assert_eq!(
            gdbserver_listen_addr("Listening on port 33785"),
            Some(ListenAddr {
                host: "127.0.0.1".into(),
                port: 33785
            })
        );
        assert_eq!(
            gdbserver_listen_addr("Process /x created; pid = 8697"),
            None
        );
    }

    #[test]
    fn finds_rr_port() {
        let line = "  'gdb' '-l' '10000' '-ex' 'set sysroot /' '-ex' \
                    'target extended-remote 127.0.0.1:45677' '/tmp/prog' ";
        assert_eq!(
            rr_listen_addr(line),
            Some(ListenAddr {
                host: "127.0.0.1".into(),
                port: 45677
            })
        );
        let v6 = "'-ex' 'target extended-remote tcp6:[::1]:9000' '/tmp/prog'";
        assert_eq!(
            rr_listen_addr(v6),
            Some(ListenAddr {
                host: "::1".into(),
                port: 9000
            })
        );
        assert_eq!(rr_listen_addr("Launch debugger with"), None);
        assert_eq!(rr_listen_addr("target extended-remote nowhere"), None);
    }

    #[test]
    fn reports_a_server_that_exits_early() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo oops >&2; exit 3"]);
        let err =
            ServerProcess::spawn(cmd, gdbserver_listen_addr, Duration::from_secs(10)).unwrap_err();
        match err {
            HopperError::Disconnected(m) => assert!(m.contains("oops"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn times_out_on_a_silent_server() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let err = ServerProcess::spawn(cmd, gdbserver_listen_addr, Duration::from_millis(100))
            .unwrap_err();
        assert!(matches!(err, HopperError::Io(m) if m.contains("no listening port")));
    }
}
