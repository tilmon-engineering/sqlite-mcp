use serde_yaml::Value;
use std::{fs, path::Path, process::Command};

fn workflow() -> Value {
    let text = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows/release.yml"
    ))
    .expect("release workflow exists");
    serde_yaml::from_str(&text).expect("release workflow is valid YAML")
}

fn mapping<'a>(value: &'a Value, key: &str) -> &'a serde_yaml::Mapping {
    value
        .get(key)
        .unwrap_or_else(|| panic!("workflow missing {key}"))
        .as_mapping()
        .unwrap_or_else(|| panic!("workflow {key} must be a mapping"))
}

fn scalar(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{key} must be a string"))
        .to_owned()
}

#[test]
fn release_workflow_contract() {
    let doc = workflow();
    assert_eq!(scalar(&doc, "name"), "Release");
    let trigger = doc.get("on").expect("workflow trigger exists");
    let branches = trigger["push"]["branches"]
        .as_sequence()
        .expect("push branches");
    assert!(branches.iter().any(|v| v.as_str() == Some("main")));
    assert_eq!(trigger["push"]["tags"][0].as_str(), Some("**"));
    assert!(
        trigger["pull_request"]["branches"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("main"))
    );
    let jobs = mapping(&doc, "jobs");
    assert!(jobs.contains_key("validate"));
    assert!(jobs.contains_key("native"));
    assert!(jobs.contains_key("publish"));
}

#[test]
fn actionlint_release_workflow() {
    let workflow_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml");
    let status = Command::new("mise")
        .args(["exec", "--", "actionlint"])
        .arg(workflow_path)
        .status()
        .expect("actionlint must be installed via mise");
    assert!(status.success(), "actionlint rejected release workflow");
}

#[test]
fn workflow_action_pins_are_exact_verified_shas() {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml"),
    )
    .unwrap();
    for (action, sha) in [
        (
            "actions/checkout",
            "3d3c42e5aac5ba805825da76410c181273ba90b1",
        ),
        (
            "actions/upload-artifact",
            "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
        ),
        (
            "actions/download-artifact",
            "3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
        ),
    ] {
        let needle = format!("{action}@{sha}");
        assert!(text.contains(&needle), "missing verified pin {needle}");
    }
    assert!(!text.contains("043fb46d1a93c77aae656e7c1c64a875d1fc6a0a0"));
}

#[test]
fn native_checkout_exact_sha_contract() {
    let native = &workflow()["jobs"]["native"];
    let steps = native["steps"].as_sequence().unwrap();
    let checkout = steps
        .iter()
        .find(|step| step["name"].as_str() == Some("Checkout"))
        .expect("native checkout step");
    assert_eq!(checkout["with"]["ref"].as_str(), Some("${{ github.sha }}"));
    let assertion = steps
        .iter()
        .find(|step| step["name"].as_str() == Some("Assert checked-out commit"))
        .expect("native HEAD assertion step");
    assert_eq!(
        assertion["env"]["EXPECTED_SHA"].as_str(),
        Some("${{ github.sha }}")
    );
    assert!(
        assertion["run"]
            .as_str()
            .unwrap()
            .contains("git rev-parse HEAD")
    );
}

#[test]
fn workflow_toolchain_actionlint_and_tag_contract() {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml"),
    )
    .unwrap();
    assert_eq!(text.matches("rustup toolchain install").count(), 3);
    assert_eq!(
        text.matches("rustup default \"$RUST_TOOLCHAIN\"").count(),
        3
    );
    assert!(text.matches("cargo +\"$RUST_TOOLCHAIN\"").count() >= 10);
    assert_eq!(text.matches("version='1.7.12'").count(), 2);
    assert!(text.contains("sha256sum --check --status"));
    assert!(text.contains("shasum -a 256 --check"));
    assert!(text.contains("RUNNER_OS: ${{ runner.os }}"));
    assert!(text.contains("REF_TYPE: ${{ github.ref_type }}"));
    assert!(text.contains("if [[ \"$REF_TYPE\" == tag ]]"));
    assert_eq!(
        text.matches("printf 'TAG=%s\\n' \"$TAG\" >> \"$GITHUB_ENV\"")
            .count(),
        2
    );
    assert!(text.contains("fetch-depth: 0"));
    assert!(text.contains("native-smoke \"$TAG\" \"$BINARY_PATH\""));
    let native = &workflow()["jobs"]["native"]["steps"];
    let runs: Vec<_> = native
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|step| step["run"].as_str())
        .collect();
    assert!(runs.iter().any(|run| run.contains("fmt --all -- --check")));
    assert!(
        runs.iter()
            .any(|run| run.contains("clippy --workspace --all-targets -- -D warnings"))
    );
    assert!(
        runs.iter()
            .any(|run| run.contains("test --workspace --locked"))
    );
}

#[test]
fn mise_release_interfaces_are_explicit() {
    let mise =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mise.toml")).unwrap();
    assert!(mise.contains("release-build"));
    assert!(mise.contains("cargo build --locked --release -p sqlite-mcp --bin sqlite-mcp"));
    assert!(mise.contains("native-smoke {{arg(name='tag')}} {{arg(name='binary')}}"));
}

#[test]
fn workflow_publish_permissions_and_event_guard() {
    let doc = workflow();
    assert_eq!(doc["permissions"]["contents"].as_str(), Some("read"));
    let publish = &doc["jobs"]["publish"];
    assert_eq!(publish["permissions"]["contents"].as_str(), Some("write"));
    assert_eq!(
        publish["if"].as_str(),
        Some("github.event_name == 'push' && startsWith(github.ref, 'refs/tags/')")
    );
    assert_eq!(publish["needs"].as_str(), Some("native"));
    assert_eq!(
        doc["concurrency"]["cancel-in-progress"].as_bool(),
        Some(false)
    );
    assert_eq!(
        doc["jobs"]["native"]["strategy"]["fail-fast"].as_bool(),
        Some(false)
    );
}

#[test]
fn workflow_exact_artifact_paths() {
    let doc = workflow();
    let matrix = doc["jobs"]["native"]["strategy"]["matrix"]["include"]
        .as_sequence()
        .expect("native matrix");
    let targets: Vec<_> = matrix.iter().map(|v| scalar(v, "target")).collect();
    assert_eq!(
        targets,
        ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]
    );
    assert_eq!(scalar(&matrix[0], "os"), "ubuntu-24.04");
    assert_eq!(scalar(&matrix[1], "os"), "macos-15");
    let upload = &doc["jobs"]["native"]["steps"];
    let upload = upload
        .as_sequence()
        .unwrap()
        .iter()
        .find(|step| {
            step["uses"]
                .as_str()
                .unwrap_or("")
                .starts_with("actions/upload-artifact@")
        })
        .expect("artifact upload step");
    assert_eq!(upload["with"]["if-no-files-found"].as_str(), Some("error"));
    assert!(
        upload["with"]["path"]
            .as_str()
            .unwrap()
            .contains("release-assets/")
    );
}

fn assert_no_ref_expressions_in_runs(value: &Value, path: &str) {
    match value {
        Value::Mapping(map) => {
            for (key, child) in map {
                let key = key.as_str().unwrap_or("<non-string-key>");
                let child_path = format!("{path}.{key}");
                if key == "run"
                    && let Some(run) = child.as_str()
                {
                    assert!(
                        !run.contains("github.ref")
                            && !run.contains("github.event")
                            && !run.contains("github.sha"),
                        "GitHub ref/event expression in shell run at {child_path}: {run}"
                    );
                }
                assert_no_ref_expressions_in_runs(child, &child_path);
            }
        }
        Value::Sequence(items) => {
            for (index, child) in items.iter().enumerate() {
                assert_no_ref_expressions_in_runs(child, &format!("{path}[{index}]"));
            }
        }
        _ => {}
    }
}

#[test]
fn workflow_ref_values_env_not_shell_source() {
    let doc = workflow();
    assert_no_ref_expressions_in_runs(&doc, "workflow");
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml"),
    )
    .unwrap();
    assert!(text.contains("REF_NAME: ${{ github.ref_name }}"));
    assert!(text.contains("REF_TYPE: ${{ github.ref_type }}"));
    assert!(text.contains("EXPECTED_SHA: ${{ github.sha }}"));
}

#[test]
fn workflow_native_archive_command_contract() {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml"),
    )
    .unwrap();
    for target in ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"] {
        assert!(text.contains(&format!("release-{target}")));
        assert!(text.contains("package \"$TAG\" \"$TARGET\" \"$BINARY_PATH\" \"$OUTPUT_DIR\""));
    }
}

#[test]
fn published_assets_check() {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml"),
    )
    .unwrap();
    assert!(text.contains("publish \"$TAG\" \"$EXPECTED_SHA\" \"$ASSET_DIR\""));
    assert_eq!(text.matches("actions/download-artifact@").count(), 2);
}
