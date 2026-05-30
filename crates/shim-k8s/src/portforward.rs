//! kubectl port-forward wrapper with Drop-based cleanup.

use shim_core::ShimError;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A live `kubectl port-forward` child process; killed on drop.
pub struct PortForward {
    child: Child,
    pub local_port: u16,
}

impl PortForward {
    /// Spawns `kubectl port-forward -n <ns> pod/<sandbox_name> 0:<remote_port>` and
    /// parses the assigned local port from stdout.
    ///
    /// Targets the pod (not the Sandbox CR or its Service): the controller
    /// names the pod identically to the Sandbox, and the auto-created Service
    /// is headless with no ports declared so `service/<name>` would fail.
    ///
    /// Returns once kubectl prints `Forwarding from 127.0.0.1:<port> -> <remote>`
    /// or after a 10-second timeout (whichever comes first).
    pub fn open(namespace: &str, sandbox_name: &str, remote_port: u16) -> Result<Self, ShimError> {
        let mut child = Command::new("kubectl")
            // The agent-sandbox controller names the pod identically to the
            // Sandbox CR. Port-forward directly to that pod — the auto-created
            // Service is headless with no ports declared, so service/<name>
            // doesn't work for port-forward.
            .args([
                "port-forward",
                "-n",
                namespace,
                &format!("pod/{sandbox_name}"),
                &format!("0:{remote_port}"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ShimError::PortForward(format!("spawn kubectl: {e}")))?;

        let Some(stdout) = child.stdout.take() else {
            return Err(ShimError::PortForward(
                "kubectl stdout pipe missing".to_string(),
            ));
        };

        let local_port = parse_port_with_timeout(stdout, Duration::from_secs(10))?;
        // kubectl prints "Forwarding from ..." before it's actually accepting
        // connections; probe TCP until a connect succeeds (or 5s elapses).
        wait_for_listener(local_port, Duration::from_secs(5))?;
        Ok(Self { child, local_port })
    }

    /// Returns the local URL the port-forward listens on (`http://127.0.0.1:<port>`).
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_port_with_timeout(
    stdout: impl std::io::Read + Send + 'static,
    timeout: Duration,
) -> Result<u16, ShimError> {
    use std::sync::mpsc;
    use std::thread;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(port) = parse_port_line(&line) {
                let _ = tx.send(port);
                return;
            }
        }
    });

    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(port) = rx.recv_timeout(Duration::from_millis(100)) {
            return Ok(port);
        }
    }
    Err(ShimError::PortForward(format!(
        "timed out after {}s waiting for port-forward to bind",
        timeout.as_secs()
    )))
}

fn wait_for_listener(port: u16, timeout: Duration) -> Result<(), ShimError> {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let start = Instant::now();
    let mut last_err = String::new();
    while start.elapsed() < timeout {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            Ok(_) => {
                // TCP connect can succeed before kubectl has the backend
                // upstream wired up; first HTTP request still races (manual
                // testing shows curl works after a 2s gap but fails earlier).
                // Give kubectl ample time to finish proxy setup before
                // returning so the first reqwest call doesn't get RST.
                std::thread::sleep(Duration::from_millis(2000));
                return Ok(());
            }
            Err(e) => {
                last_err = e.to_string();
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(ShimError::PortForward(format!(
        "port-forward bound but TCP not accepting after {}s: {}",
        timeout.as_secs(),
        last_err
    )))
}

/// Parses lines like `Forwarding from 127.0.0.1:50321 -> 43100`.
fn parse_port_line(line: &str) -> Option<u16> {
    let after_prefix = line.split("127.0.0.1:").nth(1)?;
    let port_str: String = after_prefix
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    port_str.parse::<u16>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_line_extracts_assigned_port() {
        assert_eq!(
            parse_port_line("Forwarding from 127.0.0.1:50321 -> 43100"),
            Some(50321)
        );
        assert_eq!(
            parse_port_line("Forwarding from 127.0.0.1:8080 -> 80"),
            Some(8080)
        );
    }

    #[test]
    fn parse_port_line_rejects_unrelated_lines() {
        assert_eq!(parse_port_line("some other output"), None);
        assert_eq!(parse_port_line("Forwarding from [::1]:9090 -> 80"), None);
    }
}
