#![allow(dead_code)]
pub mod harness;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

pub struct ServerProcess {
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
}
impl ServerProcess {
    pub fn spawn() -> Self {
        Self::spawn_with_args(&[])
    }
    pub fn spawn_with_args(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sqlite-mcp"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sqlite-mcp");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            stdout: rx,
            child,
        }
    }
    pub fn send(&mut self, value: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(value.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }
    pub fn send_json(&mut self, value: Value) {
        self.send(&(serde_json::to_string(&value).unwrap() + "\n"));
    }
    pub fn receive_timeout(&mut self, timeout: Duration) -> Option<Value> {
        let line = self.stdout.recv_timeout(timeout).ok()?;
        Some(
            serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("invalid JSON stdout {line:?}: {e}")),
        )
    }
    pub fn receive(&mut self) -> Value {
        self.receive_timeout(Duration::from_secs(5))
            .expect("timed out waiting for MCP stdout")
    }
    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        loop {
            let value = self.receive();
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }
    pub fn initialize(&mut self) -> Value {
        let response = self.request(1, "initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"sqlite-mcp-tests","version":"0"}}));
        self.send_json(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        response
    }
    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }
    pub fn wait_bounded(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "server did not exit within {timeout:?}"
            );
            std::thread::yield_now();
        }
    }
    pub fn stderr(&mut self) -> String {
        let mut s = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    }
}

pub fn config_file(dir: &tempfile::TempDir, body: &str) -> String {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, body).unwrap();
    path.to_str().unwrap().to_owned()
}
pub fn open_handle(server: &mut ServerProcess, id: &mut u64, path: &str) -> String {
    let open = server.request(
        *id,
        "tools/call",
        json!({"name":"open_database","arguments":{"path":path,"readonly":false}}),
    );
    *id += 1;
    open["result"]["structuredContent"]["handle_state"]["handle"]
        .as_str()
        .or_else(|| open["result"]["structuredContent"]["result"]["handle"].as_str())
        .unwrap_or_else(|| panic!("open response: {open}"))
        .to_owned()
}
pub fn error_class(response: &Value) -> &str {
    response["result"]["structuredContent"]["error"]["class"]
        .as_str()
        .unwrap_or("")
}
pub fn assert_ok(response: &Value) {
    assert!(
        response["result"]["structuredContent"].is_object(),
        "response: {response}"
    );
    assert_ne!(
        response["result"]["isError"], true,
        "expected success, got error envelope: {response}"
    );
}
pub fn request_tool(server: &mut ServerProcess, id: &mut u64, name: &str, args: Value) -> Value {
    let response = server.request(*id, "tools/call", json!({"name":name,"arguments":args}));
    *id += 1;
    response
}
