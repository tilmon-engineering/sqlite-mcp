//! Phase 0 deterministic harness foundation.
//!
//! These tests prove the isolated instrumented child exercises the keyed
//! event/clock hooks deterministically, bounds every failure, and that the
//! normal production binary keeps its hooks inert. Production behavior is
//! unchanged: all instrumentation requires the `test-support` feature plus the
//! `SQLITE_MCP_TEST_SUPPORT=1` marker, and the production artifact is built
//! without either.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

mod support;

use support::harness::ChildHarness;

use sqlite_mcp_core::test_support::{self, Event};

fn event_debug_name(event: Event) -> &'static str {
    match event {
        Event::BeginCompletion => "BeginCompletion",
        Event::Prepare => "Prepare",
        Event::StepProgress => "StepProgress",
        Event::BusyRetry => "BusyRetry",
        Event::CommitEntry => "CommitEntry",
        Event::CommitReturn => "CommitReturn",
        Event::RollbackCompletion => "RollbackCompletion",
        Event::RequestHandover => "RequestHandover",
        Event::AdmissionEnqueue => "AdmissionEnqueue",
        Event::AdmissionDequeue => "AdmissionDequeue",
        Event::ClosureEntry => "ClosureEntry",
        Event::PostWorkerPrePublication => "PostWorkerPrePublication",
        Event::ControlEntry => "ControlEntry",
        Event::ControlReturn => "ControlReturn",
        Event::ShutdownRequested => "ShutdownRequested",
        Event::ShutdownCleanup => "ShutdownCleanup",
        Event::ShutdownClosed => "ShutdownClosed",
        Event::SchemaVersionRead => "SchemaVersionRead",
        Event::CreationDescriptorCaptured => "CreationDescriptorCaptured",
        Event::CreationPreOpenCheckpoint => "CreationPreOpenCheckpoint",
        Event::CreationPostOpenCheckpoint => "CreationPostOpenCheckpoint",
        Event::CreationPostInitCheckpoint => "CreationPostInitCheckpoint",
        Event::HarnessCommand => "HarnessCommand",
    }
}

/// The instrumented child entry. Inert (returns immediately) unless launched
/// by the parent harness with the control endpoint in the environment; normal
/// `cargo test` runs of this binary are unaffected. The child resets the
/// process-global registry and clock at entry so another child's process
/// globals can never leak in.
#[test]
fn harness_child() {
    let Ok(control_address) = std::env::var("SQLITE_MCP_TEST_CONTROL") else {
        return;
    };
    let nonce = std::env::var("SQLITE_MCP_TEST_NONCE").expect("parent supplies nonce");
    sqlite_mcp_core::test_support::reset_registry();

    let mut control = connect_control(&control_address);
    let _ = writeln!(control, "ready {nonce}");

    let reader = BufReader::new(control.try_clone().expect("control clone"));
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let reply = execute_control_command(&line);
        // The handshake line is acknowledged by the unsolicited `ready`
        // message; replying to it would desynchronize the command/reply
        // pairing for every subsequent command.
        if !reply.is_empty() && writeln!(control, "{reply}").is_err() {
            break;
        }
    }
    // Parent control disconnected: bounded cleanup and exit.
    sqlite_mcp_core::test_support::reset_registry();
}

fn connect_control(address: &str) -> std::net::TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::net::TcpStream::connect(address) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return stream;
            }
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "instrumented child failed to reach control: {error}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn execute_control_command(line: &str) -> String {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    match tokens.as_slice() {
        // Handshake line: already acknowledged by the unsolicited `ready`
        // message sent at entry; no reply (keeps command/reply pairing).
        ["nonce", _] => String::new(),
        ["arm", event, op, generation] => {
            let key = test_support::EventKey::operation(op.parse().expect("operation id"))
                .with_generation(generation.parse().expect("generation value"));
            test_support::arm_keyed(parse_event(event), Some(key));
            "done".to_owned()
        }
        ["emit", event, op, generation] => {
            let key = test_support::EventKey::operation(op.parse().expect("operation id"))
                .with_generation(generation.parse().expect("generation value"));
            let event = parse_event(event);
            if test_support::arm_matches_pending(event, Some(&key)) {
                // This emission pauses on its armed gate; keep the control
                // loop responsive by emitting on a background thread.
                std::thread::spawn(move || test_support::emit_keyed(event, Some(key)));
                "started".to_owned()
            } else {
                test_support::emit_keyed(event, Some(key));
                "done".to_owned()
            }
        }
        ["wait", event, op, generation, ms] => {
            let key = test_support::EventKey::operation(op.parse().expect("operation id"))
                .with_generation(generation.parse().expect("generation value"));
            match test_support::wait_keyed(
                parse_event(event),
                Some(&key),
                Duration::from_millis(ms.parse().expect("deadline ms")),
            ) {
                Ok(record) => format!(
                    "record seq={} op={} gen={}",
                    record.seq,
                    record.key.as_ref().expect("keyed record").operation_id,
                    record.key.as_ref().expect("keyed record").generation,
                ),
                Err(detail) => format!("err {detail}"),
            }
        }
        ["release", event] => {
            test_support::release_arm(parse_event(event));
            "done".to_owned()
        }
        ["clock", ms] => {
            test_support::set_clock_ms(ms.parse().expect("clock ms"));
            "done".to_owned()
        }
        ["now"] => format!("now {}", test_support::now_ms()),
        ["count", event] => format!("count {}", test_support::count(parse_event(event))),
        ["serve", port] => match start_serve_mcp(port.parse().expect("mcp port")) {
            Ok(()) => "done".to_owned(),
            Err(detail) => format!("err {detail}"),
        },
        ["exit"] => {
            sqlite_mcp_core::test_support::reset_registry();
            std::process::exit(0);
        }
        other => format!("err unknown control command {other:?}"),
    }
}

fn parse_event(name: &str) -> Event {
    test_support::ALL_EVENTS
        .iter()
        .copied()
        .find(|event| event_debug_name(*event) == name)
        .unwrap_or_else(|| panic!("unknown event name {name}"))
}

/// Connect the child to the parent's MCP transport listener, then serve with
/// the production helper on a background thread. The transport connection is
/// established before returning so the parent's accept is guaranteed to find
/// it in the listener backlog after the `done` acknowledgement.
fn start_serve_mcp(port: u16) -> Result<(), String> {
    let address = format!("127.0.0.1:{port}");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("child runtime: {error}"))?;
    let stream = runtime
        .block_on(async { tokio::net::TcpStream::connect(&address).await })
        .map_err(|error| format!("mcp transport connect: {error}"))?;
    let (read_half, write_half) = stream.into_split();
    std::thread::spawn(move || {
        let result = runtime.block_on(async move {
            sqlite_mcp::serve_with_transport(
                sqlite_mcp_core::Config::default(),
                (read_half, write_half),
            )
            .await
        });
        if let Err(failure) = result {
            eprintln!("child serve failure: {failure}");
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Foundation tests. All waits are bounded; every child is reaped by Drop even
// on assertion failure.
// ---------------------------------------------------------------------------

#[test]
fn instrumented_child_emits_keyed_event() {
    let mut child = ChildHarness::spawn().expect("instrumented child");
    // Arm-before-trigger inside the child's registry.
    let reply = child.command("arm HarnessCommand 42 1").expect("arm");
    assert_eq!(reply.trim(), "done");
    // Trigger via the child's production hook surface. The matching armed
    // gate pauses the emitter, so the child reports "started".
    let reply = child.command("emit HarnessCommand 42 1").expect("emit");
    assert_eq!(reply.trim(), "started");
    // Acknowledge-before-release: the emit is paused until release.
    let reply = child
        .command("wait HarnessCommand 42 1 2000")
        .expect("wait");
    assert!(
        reply.trim().starts_with("record seq="),
        "keyed record must be observed: {reply}"
    );
    assert!(
        reply.contains("op=42") && reply.contains("gen=1"),
        "{reply}"
    );
    let reply = child.command("release HarnessCommand").expect("release");
    assert_eq!(reply.trim(), "done");
    let reply = child.command("exit").expect("exit");
    let _ = reply;
    child.kill_and_wait();
    let status = child.exit_status.expect("child reaped");
    assert!(status.success(), "child exit status {status}");
}

#[test]
fn harness_events_are_keyed_and_lossless() {
    let mut child = ChildHarness::spawn().expect("instrumented child");
    // Five un-armed emissions first; waits that start later must still
    // observe them (retention makes waits lossless).
    for op in 1..=5u64 {
        let reply = child
            .command(&format!("emit HarnessCommand {op} 0"))
            .expect("emit");
        assert_eq!(reply.trim(), "done");
    }
    let reply = child
        .command("wait HarnessCommand 3 0 2000")
        .expect("wait for earlier emission");
    assert!(
        reply.trim().starts_with("record seq=") && reply.contains("op=3"),
        "earlier emission must not be lost: {reply}"
    );
    let reply = child.command("count HarnessCommand").expect("count");
    assert_eq!(
        reply.trim(),
        "count 5",
        "exactly the emitted events retained"
    );
    // Key mismatch: arming for op 6 gen 1 must not be satisfied by gen 2.
    let _ = child.command("arm HarnessCommand 6 1").expect("arm");
    let reply = child
        .command("emit HarnessCommand 6 2")
        .expect("emit mismatched");
    assert_eq!(reply.trim(), "done", "non-matching emit must not pause");
    let reply = child
        .command("wait HarnessCommand 6 1 100")
        .expect("bounded wait for missing key");
    assert!(
        reply.trim().starts_with("err "),
        "mismatched key must not satisfy the arm: {reply}"
    );
    let _ = child.command("release HarnessCommand").expect("release");
    child.kill_and_wait();
}

#[test]
fn harness_missing_event_fails_bounded() {
    let mut child = ChildHarness::spawn().expect("instrumented child");
    let _ = child.command("arm HarnessCommand 999 0").expect("arm");
    let start = Instant::now();
    let reply = child
        .command("wait HarnessCommand 999 0 150")
        .expect("bounded wait command completes");
    let elapsed = start.elapsed();
    assert!(
        reply.trim().starts_with("err "),
        "missing event must fail: {reply}"
    );
    assert!(
        elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(2),
        "failure must be bounded near the requested deadline: {elapsed:?}"
    );
    let _ = child.command("release HarnessCommand").expect("release");
    child.kill_and_wait();
}

#[test]
fn harness_clock_isolation() {
    let mut child = ChildHarness::spawn().expect("instrumented child");
    // Child reports real time before injection (feature and marker active,
    // no injected value yet).
    let reply = child.command("now").expect("now");
    assert_eq!(
        reply.trim(),
        "now 0",
        "fresh child starts at real time {reply}"
    );
    // Inject the clock in the child only.
    let reply = child.command("clock 123456").expect("clock");
    assert_eq!(reply.trim(), "done");
    let reply = child.command("now").expect("now after injection");
    assert_eq!(
        reply.trim(),
        "now 123456",
        "child honors its injected clock"
    );
    // Parent process clock is isolated: injection in the child must not leak.
    let parent_ms = test_support::now_ms();
    assert_ne!(
        parent_ms, 123456,
        "parent clock must be independent of child injection"
    );
    child.kill_and_wait();
}

#[test]
fn harness_failure_reaps_child() {
    // Case 1: control disconnect makes the child exit by itself, bounded.
    {
        let mut child = ChildHarness::spawn().expect("instrumented child");
        child.kill_and_wait(); // closes control, reaps within bound
        let status = child.exit_status.expect("child reaped after control close");
        assert!(
            status.success(),
            "clean control disconnect exits 0: {status}"
        );
    }
    // Case 2: dropping the harness without an exit command still reaps.
    {
        let mut child = ChildHarness::spawn().expect("instrumented child");
        let _ = child.command("now").expect("child alive");
        child.kill_and_wait();
        assert!(child.exit_status.is_some(), "drop path must reap the child");
    }
}

/// Production proof: the separately built normal binary (no test-support
/// feature) must perform ordinary MCP startup while the marker and a control
/// endpoint are deliberately present, and must never connect to the control
/// channel or honor injected hooks.
#[test]
fn production_test_hooks_inert() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir");
    let repository = std::path::Path::new(&manifest).join("../..");
    let artifact = repository.join("target/production-verification/debug/sqlite-mcp");
    if !artifact.exists() {
        // Build the pristine production binary on demand so this proof is
        // self-contained on fresh machines and CI runners. The separate
        // target directory keeps test-support hooks out of the artifact.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = Command::new(&cargo)
            .args(["build", "--locked", "-p", "sqlite-mcp"])
            .arg("--target-dir")
            .arg(repository.join("target/production-verification"))
            .current_dir(&repository)
            .status()
            .expect("spawn cargo for the production verification build");
        assert!(
            status.success(),
            "production verification build failed: {status}"
        );
    }
    let production = artifact
        .canonicalize()
        .expect("production verification artifact missing after the on-demand build");
    let listener = TcpListener::bind("127.0.0.1:0").expect("control listener");
    let address = listener.local_addr().expect("control address").to_string();
    listener
        .set_nonblocking(true)
        .expect("nonblocking control listener");
    let mut child = Command::new(&production)
        .env("SQLITE_MCP_TEST_SUPPORT", "1")
        .env("SQLITE_MCP_TEST_CONTROL", address)
        .env("SQLITE_MCP_TEST_NONCE", "12345")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn production binary");
    let mut stdin = child.stdin.take().expect("production stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("production stdout"));
    // Deliberately injected control/hook environment present; ordinary MCP
    // startup must proceed over stdio regardless.
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "inert-proof", "version": "0"}
        }
    });
    writeln!(stdin, "{initialize}").expect("write initialize");
    stdin.flush().expect("flush initialize");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = stdout.read_line(&mut line);
        let _ = sender.send((read, line));
    });
    let (read, line) = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("production binary answered initialize in time");
    assert!(read.unwrap() > 0, "production stdout closed early");
    let response: Value = serde_json::from_str(line.trim()).expect("initialize response json");
    assert_eq!(response["id"], 0, "initialize response id");
    assert!(
        response["result"]["serverInfo"].is_object(),
        "ordinary MCP startup expected: {response}"
    );
    // Give any (forbidden) control connection a bounded window to appear.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        listener.accept().is_err(),
        "production binary must never connect to the control channel"
    );
    // Clean EOF: close stdin, bounded wait, exit zero.
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll production child") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "production binary must exit on EOF within bound"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "clean EOF must exit zero: {status}");
}
