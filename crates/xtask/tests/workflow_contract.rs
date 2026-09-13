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

fn scalar(value: &Value, key: &str) -> String {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} must be a string"))
        .to_owned()
}

fn steps(job: &Value) -> &Vec<Value> {
    job["steps"].as_sequence().expect("job steps")
}

const ARTIFACTS: [&str; 4] = [
    "release-x86_64-unknown-linux-gnu",
    "release-aarch64-unknown-linux-gnu",
    "release-aarch64-apple-darwin",
    "release-x86_64-apple-darwin",
];
const TUPLES: [(&str, &str, &str, &str); 4] = [
    (
        "ubuntu-24.04",
        "x86_64-unknown-linux-gnu",
        "x86_64",
        "linux_amd64",
    ),
    (
        "ubuntu-24.04-arm",
        "aarch64-unknown-linux-gnu",
        "aarch64",
        "linux_arm64",
    ),
    ("macos-15", "aarch64-apple-darwin", "arm64", "darwin_arm64"),
    (
        "macos-15-intel",
        "x86_64-apple-darwin",
        "x86_64",
        "darwin_amd64",
    ),
];

#[test]
fn release_workflow_contract() {
    let doc = workflow();
    assert_eq!(scalar(&doc, "name"), "Release");
    assert!(
        doc["on"]["push"]["branches"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("main"))
    );
    assert_eq!(doc["on"]["push"]["tags"][0].as_str(), Some("**"));
    let jobs = doc["jobs"].as_mapping().unwrap();
    for job in [
        "validate",
        "native",
        "publish",
        "verify-published",
        "verify-release-metadata",
    ] {
        assert!(
            jobs.contains_key(Value::String(job.into())),
            "missing {job}"
        );
    }
}

#[test]
fn actionlint_release_workflow() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml");
    // CI jobs install actionlint onto PATH before running the suite; local
    // development reaches it through the pinned mise toolchain.
    let direct = Command::new("actionlint").arg(&path).status();
    let status = match direct {
        Ok(status) => status,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Command::new("mise")
            .args(["exec", "--", "actionlint"])
            .arg(path)
            .status()
            .expect("actionlint available directly or through mise"),
        Err(error) => panic!("failed to run actionlint: {error}"),
    };
    assert!(status.success());
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
        assert!(text.contains(&format!("{action}@{sha}")));
    }
}

#[test]
fn workflow_four_platform_matrix_contract() {
    let doc = workflow();
    let matrix = doc["jobs"]["native"]["strategy"]["matrix"]["include"]
        .as_sequence()
        .unwrap();
    let actual: Vec<_> = matrix
        .iter()
        .map(|v| {
            (
                scalar(v, "os"),
                scalar(v, "target"),
                scalar(v, "arch"),
                scalar(v, "actionlint_suffix"),
            )
        })
        .collect();
    assert_eq!(
        actual,
        TUPLES
            .map(|(a, b, c, d)| (a.into(), b.into(), c.into(), d.into()))
            .to_vec()
    );
    let uploads: Vec<_> = steps(&doc["jobs"]["native"])
        .iter()
        .filter_map(|s| s["with"]["name"].as_str())
        .collect();
    assert_eq!(uploads, vec!["release-${{ matrix.target }}"]);
    assert_eq!(ARTIFACTS.len(), 4);
}

#[test]
fn workflow_actionlint_platform_contract() {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml"),
    )
    .unwrap();
    for suffix in ["linux_amd64", "linux_arm64", "darwin_arm64", "darwin_amd64"] {
        assert!(text.contains(&format!("actionlint_suffix: {suffix}")));
    }
    assert!(text.contains("sha256sum") && text.contains("shasum -a 256"));
    assert!(text.contains("ACTIONLINT_CHECKSUM"));
    assert!(text.contains("ACTIONLINT_RUNNER_ARCH"));
    assert!(text.contains("unsupported actionlint runner tuple"));
    assert_eq!(
        text.matches("--proto '=https' --proto-redir '=https'")
            .count(),
        4
    );
    assert!(
        text.contains("readelf -h")
            && text.contains("readelf -l")
            && text.contains("otool -l")
            && text.contains("otool -L")
            && text.contains("runtime_env_defaults")
            && text.contains("uname -m")
    );
    assert!(!text.contains("Inspect Linux runtime"));
}

#[test]
fn workflow_publish_gates_contract() {
    let doc = workflow();
    let publish = &doc["jobs"]["publish"];
    assert_eq!(publish["needs"].as_str(), Some("native"));
    assert_eq!(
        publish["if"].as_str(),
        Some("github.event_name == 'push' && startsWith(github.ref, 'refs/tags/')")
    );
    assert_eq!(publish["permissions"]["contents"].as_str(), Some("write"));
    let names: Vec<_> = steps(publish)
        .iter()
        .filter_map(|s| s["with"]["name"].as_str())
        .collect();
    assert_eq!(names, ARTIFACTS);
}

#[test]
fn workflow_public_download_verification_contract() {
    let doc = workflow();
    let job = &doc["jobs"]["verify-published"];
    assert_eq!(job["needs"].as_str(), Some("publish"));
    assert_eq!(
        job["if"].as_str(),
        Some("github.event_name == 'push' && startsWith(github.ref, 'refs/tags/')")
    );
    assert_eq!(job["permissions"]["contents"].as_str(), Some("read"));
    let matrix: Vec<_> = job["strategy"]["matrix"]["include"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| (scalar(v, "os"), scalar(v, "target")))
        .collect();
    assert_eq!(
        matrix,
        TUPLES.map(|(a, b, _, _)| (a.into(), b.into())).to_vec()
    );
    let ss = steps(job);
    let checkout = ss
        .iter()
        .find(|s| {
            s["uses"]
                .as_str()
                .unwrap_or("")
                .starts_with("actions/checkout@")
        })
        .unwrap();
    assert_eq!(checkout["with"]["ref"].as_str(), Some("${{ github.sha }}"));
    let assertion = ss
        .iter()
        .find(|s| s["name"].as_str() == Some("Assert exact published commit"))
        .unwrap();
    assert!(
        assertion["run"]
            .as_str()
            .unwrap()
            .contains("git rev-parse HEAD")
            && assertion["env"]["EXPECTED_SHA"].as_str() == Some("${{ github.sha }}")
    );
    let run: String = ss
        .iter()
        .filter_map(|s| s["run"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let job_text = serde_yaml::to_string(job).unwrap();
    for needle in [
        "fetch-public-assets",
        "verify-downloads",
        "tar -tzf",
        "mkdir \"$EXTRACT_DIR\"",
        "runner.temp",
        "sqlite-mcp-assets",
    ] {
        assert!(
            run.contains(needle) || job_text.contains(needle),
            "missing {needle}"
        );
    }
    assert!(
        !run.contains("target/")
            && !run.contains("GH_TOKEN")
            && !run.contains("GITHUB_TOKEN")
            && !run.contains("Authorization")
    );
    assert!(!run.contains("rm -rf"));
    assert_eq!(job["timeout-minutes"].as_i64(), Some(20));
}

#[test]
fn workflow_public_metadata_contract() {
    let doc = workflow();
    let job = &doc["jobs"]["verify-release-metadata"];
    assert_eq!(job["needs"].as_str(), Some("publish"));
    assert_eq!(
        job["if"].as_str(),
        Some("github.event_name == 'push' && startsWith(github.ref, 'refs/tags/')")
    );
    assert_eq!(job["permissions"]["contents"].as_str(), Some("read"));
    let ss = steps(job);
    let checkout = ss
        .iter()
        .find(|s| {
            s["uses"]
                .as_str()
                .unwrap_or("")
                .starts_with("actions/checkout@")
        })
        .unwrap();
    assert_eq!(checkout["with"]["ref"].as_str(), Some("${{ github.sha }}"));
    let run: String = ss
        .iter()
        .filter_map(|s| s["run"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(run.contains("verify-public-release \"$TAG\" \"$EXPECTED_SHA\" \"$EVIDENCE_PATH\""));
    let upload = ss
        .iter()
        .find(|s| s["with"]["name"].as_str() == Some("release-public-metadata"))
        .unwrap();
    assert_eq!(upload["id"].as_str(), Some("upload-public-evidence"));
    let job_text = serde_yaml::to_string(job).unwrap();
    assert!(job_text.contains("GITHUB_STEP_SUMMARY") && job_text.contains("artifact-url"));
    assert_eq!(job["timeout-minutes"].as_i64(), Some(20));
}

fn assert_no_ref_expressions_in_runs(value: &Value) {
    match value {
        Value::Mapping(map) => {
            for (k, v) in map {
                if k.as_str() == Some("run") {
                    let run = v.as_str().unwrap_or("");
                    assert!(
                        !run.contains("github.ref")
                            && !run.contains("github.event")
                            && !run.contains("github.sha")
                    );
                }
                assert_no_ref_expressions_in_runs(v);
            }
        }
        Value::Sequence(xs) => {
            for x in xs {
                assert_no_ref_expressions_in_runs(x);
            }
        }
        _ => {}
    }
}

#[test]
fn workflow_ref_values_env_not_shell_source() {
    assert_no_ref_expressions_in_runs(&workflow());
}
