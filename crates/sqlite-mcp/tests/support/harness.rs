//! Parent-side driver for the isolated instrumented child.
//!
//! The child is the current integration-test executable running the exact
//! `harness_child` entry with `SQLITE_MCP_TEST_SUPPORT=1` and a loopback
//! control endpoint in its environment. The parent performs a nonce/ready
//! handshake before arming or triggering anything, owns all sockets and the
//! child process, and bounds every wait: on any failure it closes the control
//! channel, kills, and reaps the child.
//!
//! MCP transport is a separate dedicated loopback connection handed to the
//! production serving helper inside the child, so libtest status text can
//! never contaminate protocol bytes.
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CONNECT_ATTEMPTS: usize = 250;
const CONNECT_SLEEP: Duration = Duration::from_millis(20);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

pub struct ChildHarness {
    child: Child,
    control_reader: Option<BufReader<TcpStream>>,
    control_writer: Option<TcpStream>,
    nonce: u64,
    pub exit_status: Option<std::process::ExitStatus>,
}

fn poll<F, T>(mut attempt: F, deadline: Duration) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    let start = Instant::now();
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) => {
                if start.elapsed() >= deadline {
                    return Err(error);
                }
                std::thread::sleep(CONNECT_SLEEP);
            }
        }
    }
}

fn readline(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut line = String::new();
    let mut reader = BufReader::new(stream.try_clone()?);
    reader.read_line(&mut line)?;
    Ok(line)
}

impl ChildHarness {
    /// Spawn the instrumented child and perform the nonce/ready handshake.
    /// On any failure the child is killed and reaped before the error returns.
    pub fn spawn() -> std::io::Result<ChildHarness> {
        let nonce = unique_nonce();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?.to_string();
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .args([
                "harness_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SQLITE_MCP_TEST_SUPPORT", "1")
            .env("SQLITE_MCP_TEST_CONTROL", address)
            .env("SQLITE_MCP_TEST_NONCE", nonce.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let accept = || listener.accept().map(|(stream, _)| stream);
        let control = match poll(accept, IO_TIMEOUT) {
            Ok(stream) => stream,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::other(format!(
                    "instrumented child never connected to control: {error}"
                )));
            }
        };
        let control_reader = control.try_clone().ok().map(BufReader::new);
        let control_writer = control.try_clone().ok();
        let mut harness = ChildHarness {
            child,
            control_reader,
            control_writer,
            nonce,
            exit_status: None,
        };
        // Nonce/ready handshake before any arming or triggering.
        if let Err(error) = harness.send(&format!("nonce {nonce}")) {
            harness.abort("handshake write failed");
            return Err(error);
        }
        let reply = match harness.receive() {
            Ok(reply) => reply,
            Err(error) => {
                harness.abort("handshake read failed");
                return Err(error);
            }
        };
        if reply.trim() != format!("ready {nonce}") {
            harness.abort("nonce mismatch");
            return Err(std::io::Error::other(format!(
                "instrumented child handshake mismatch: {reply:?}"
            )));
        }
        Ok(harness)
    }

    fn abort(&mut self, reason: &str) {
        eprintln!("child harness aborted: {reason}");
        self.kill_and_wait();
    }

    fn send(&mut self, line: &str) -> std::io::Result<()> {
        let stream = self
            .control_writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("control channel closed"))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()
    }

    fn receive(&mut self) -> std::io::Result<String> {
        // One persistent buffered reader for the connection lifetime:
        // recreating it per read would discard prefetched replies.
        let reader = self
            .control_reader
            .as_mut()
            .ok_or_else(|| std::io::Error::other("control channel closed"))?;
        reader.get_mut().set_read_timeout(Some(IO_TIMEOUT))?;
        let mut line = String::new();
        reader.read_line(&mut line)?;
        Ok(line)
    }

    /// Send a control command and read its acknowledgement.
    pub fn command(&mut self, line: &str) -> std::io::Result<String> {
        self.send(line)?;
        self.receive()
    }

    /// Open the dedicated MCP transport connection and hand it to the child's
    /// production serving helper. Returns the parent side of the transport.
    pub fn open_mcp_transport(&mut self) -> std::io::Result<TcpStream> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let reply = self.command(&format!("serve {port}"))?;
        if reply.trim() != "done" {
            return Err(std::io::Error::other(format!(
                "child refused serve command: {reply:?}"
            )));
        }
        let accept = || listener.accept().map(|(stream, _)| stream);
        let stream = poll(accept, IO_TIMEOUT)?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    /// Close control and reap the child within a bounded window, killing if
    /// necessary. Records the exit status for caller assertions.
    pub fn kill_and_wait(&mut self) {
        self.control_reader = None; // signals the child to perform bounded cleanup
        self.control_writer = None;
        let deadline = Instant::now() + REAP_TIMEOUT;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.exit_status = Some(status);
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        // Escalate to kill, then still bound the final reap.
        let _ = self.child.kill();
        let kill_deadline = Instant::now() + REAP_TIMEOUT;
        while Instant::now() < kill_deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.exit_status = Some(status);
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
    }

    /// Assert the child has been reaped (used by the failure-path test).
    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for ChildHarness {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

fn unique_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or(0);
    (nanos << 16)
        ^ (std::process::id() as u64) << 8
        ^ COUNTER.fetch_add(1, Ordering::SeqCst)
        ^ 0x5eed_0000
}
