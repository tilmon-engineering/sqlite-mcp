use std::process::Command;

fn run_version(arg: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sqlite-mcp"))
        .arg(arg)
        .output()
        .unwrap_or_else(|error| panic!("spawn sqlite-mcp {arg}: {error}"))
}

#[test]
fn cli_version_long() {
    let output = run_version("--version");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("sqlite-mcp {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {:?}",
        output.stderr
    );
}

#[test]
fn cli_version_short() {
    let output = run_version("-V");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("sqlite-mcp {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {:?}",
        output.stderr
    );
}

#[test]
fn cli_version_ignores_runtime_env() {
    let output = Command::new(env!("CARGO_BIN_EXE_sqlite-mcp"))
        .arg("--version")
        .env("CARGO_PKG_VERSION", "9.9.9-runtime-override")
        .output()
        .expect("spawn sqlite-mcp with runtime version override");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("sqlite-mcp {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {:?}",
        output.stderr
    );
}
