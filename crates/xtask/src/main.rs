use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseMetadata {
    pub version: String,
    pub tag: String,
}

pub fn expected_tag(version: &str) -> String {
    format!("v{version}")
}

pub fn validate_tag(tag: &str, version: &str) -> Result<(), String> {
    let expected = expected_tag(version);
    let valid_version = version.split('.').count() == 3
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
        && version.as_bytes().first().is_some_and(u8::is_ascii_digit);
    if valid_version && tag == expected {
        Ok(())
    } else {
        Err(format!(
            "invalid release tag {tag:?}; expected {expected:?}"
        ))
    }
}

#[derive(Debug, Deserialize)]
struct RootManifest {
    workspace: Workspace,
}
#[derive(Debug, Deserialize)]
struct Workspace {
    package: Package,
    members: Vec<String>,
}
#[derive(Debug, Deserialize)]
struct Package {
    version: String,
}
fn manifest_version(path: &Path) -> Result<(String, bool), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc: toml::Value =
        toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let package = doc
        .get("package")
        .and_then(|v| v.as_table())
        .ok_or_else(|| format!("{} has no package table", path.display()))?;
    let v = package.get("version");
    let inherited = v
        .and_then(|x| x.get("workspace"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let literal = v.and_then(|x| x.as_str()).unwrap_or("").to_owned();
    Ok((literal, inherited))
}

fn lock_versions(lock: &str) -> Result<Vec<(String, String)>, String> {
    #[derive(Deserialize)]
    struct Lock {
        package: Vec<LockPackage>,
    }
    #[derive(Deserialize)]
    struct LockPackage {
        name: String,
        version: String,
    }
    let parsed: Lock = toml::from_str(lock).map_err(|e| format!("parse Cargo.lock: {e}"))?;
    Ok(parsed
        .package
        .into_iter()
        .map(|p| (p.name, p.version))
        .collect())
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
}
#[derive(Debug, Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    version: String,
    manifest_path: PathBuf,
}

pub fn validate_repository(root: &Path, tag: &str) -> Result<ReleaseMetadata, String> {
    let root_text = fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| format!("read root Cargo.toml: {e}"))?;
    let root_manifest: RootManifest =
        toml::from_str(&root_text).map_err(|e| format!("parse root Cargo.toml: {e}"))?;
    let version = root_manifest.workspace.package.version;
    validate_tag(tag, &version)?;
    for expected in ["crates/sqlite-mcp", "crates/sqlite-mcp-core"] {
        if !root_manifest
            .workspace
            .members
            .iter()
            .any(|m| m == expected)
        {
            return Err(format!("workspace missing {expected}"));
        }
        let (literal, inherited) = manifest_version(&root.join(expected).join("Cargo.toml"))?;
        if !inherited || !literal.is_empty() {
            return Err(format!(
                "{expected}/Cargo.toml must use exactly version.workspace = true"
            ));
        }
    }
    let lock =
        fs::read_to_string(root.join("Cargo.lock")).map_err(|e| format!("read Cargo.lock: {e}"))?;
    let versions = lock_versions(&lock)?;
    for package in ["sqlite-mcp", "sqlite-mcp-core"] {
        let matches: Vec<_> = versions.iter().filter(|(n, _)| n == package).collect();
        if matches.len() != 1 || matches[0].1 != version {
            return Err(format!(
                "Cargo.lock entry for {package} does not match workspace version {version}"
            ));
        }
    }
    let output = Command::new("cargo")
        .args(["metadata", "--locked", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("run cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata --locked failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let metadata: CargoMetadata =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("parse cargo metadata: {e}"))?;
    for expected in ["crates/sqlite-mcp", "crates/sqlite-mcp-core"] {
        let manifest = root
            .join(expected)
            .join("Cargo.toml")
            .canonicalize()
            .map_err(|e| format!("canonicalize {expected}: {e}"))?;
        let name = Path::new(expected)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let package = metadata
            .packages
            .iter()
            .find(|p| {
                p.name == name && p.manifest_path.canonicalize().ok().as_ref() == Some(&manifest)
            })
            .ok_or_else(|| format!("cargo metadata missing {expected}"))?;
        if package.version != version
            || !metadata
                .workspace_members
                .iter()
                .any(|id| id == &package.id)
        {
            return Err(format!(
                "cargo metadata entry for {expected} does not match workspace version or membership"
            ));
        }
    }
    Ok(ReleaseMetadata {
        tag: tag.to_owned(),
        version,
    })
}

fn fence_marker(line: &str) -> Option<(u8, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = *rest.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let count = rest.bytes().take_while(|b| *b == ch).count();
    (count >= 3).then_some((ch, count))
}
fn valid_calendar_date(date: &str) -> bool {
    if date.len() != 10
        || date.as_bytes()[4] != b'-'
        || date.as_bytes()[7] != b'-'
        || !date
            .chars()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return false;
    }
    let y: u32 = date[0..4].parse().unwrap();
    let m: u32 = date[5..7].parse().unwrap();
    let d: u32 = date[8..10].parse().unwrap();
    let days = [
        31,
        if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
            29
        } else {
            28
        },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    (1..=12).contains(&m) && (1..=days[m as usize - 1]).contains(&d)
}
pub fn extract_notes(changelog: &str, version: &str) -> Result<String, String> {
    let lines: Vec<&str> = changelog.lines().collect();
    if lines.first().copied() != Some("# Changelog") {
        return Err("changelog must begin with # Changelog".into());
    }
    let mut fence = None;
    let mut starts = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some((ch, len)) = fence {
            if let Some((close_ch, close_len)) = fence_marker(line)
                && ch == close_ch
                && close_len >= len
                && line
                    .trim_start()
                    .chars()
                    .skip(close_len)
                    .all(char::is_whitespace)
            {
                fence = None;
            }
        } else if let Some(marker) = fence_marker(line) {
            fence = Some(marker);
        } else if line.starts_with("## ") {
            starts.push((i, *line));
        } else if line.starts_with("##") && !line.starts_with("###") {
            return Err("malformed level-two heading".into());
        }
    }
    if fence.is_some() {
        return Err("unterminated fenced block".into());
    }
    let mut releases = Vec::new();
    for (i, heading) in &starts {
        if *heading == "## [Unreleased]" {
            releases.push((*i, None));
            continue;
        }
        let rest = heading
            .strip_prefix("## [")
            .ok_or("malformed release heading")?;
        let close = rest.find("] - ").ok_or("malformed release heading")?;
        let found_version = &rest[..close];
        let date = &rest[close + 4..];
        if found_version.is_empty() || !valid_calendar_date(date) {
            return Err("release heading date must be a valid YYYY-MM-DD".into());
        }
        releases.push((*i, Some(found_version)));
    }
    let found: Vec<_> = releases
        .iter()
        .filter(|(_, v)| v.as_deref() == Some(version))
        .collect();
    if found.len() != 1 {
        return Err(format!(
            "expected exactly one changelog section for {version}"
        ));
    }
    let start = found[0].0;
    let end = starts
        .iter()
        .find(|(i, _)| *i > start)
        .map(|(i, _)| *i)
        .unwrap_or(lines.len());
    let body = lines[start + 1..end].join("\n");
    if body.trim().is_empty() {
        return Err("changelog release section is empty".into());
    }
    Ok(format!("{}\n", body.trim_end()))
}

pub fn release_notes(root: &Path, tag: &str) -> Result<String, String> {
    let meta = validate_repository(root, tag)?;
    let text = fs::read_to_string(root.join("CHANGELOG.md"))
        .map_err(|e| format!("read CHANGELOG.md: {e}"))?;
    extract_notes(&text, &meta.version)
}

pub fn archive_names() -> [&'static str; 4] {
    [
        "sqlite-mcp-x86_64-unknown-linux-gnu.tar.gz",
        "sqlite-mcp-aarch64-unknown-linux-gnu.tar.gz",
        "sqlite-mcp-aarch64-apple-darwin.tar.gz",
        "sqlite-mcp-x86_64-apple-darwin.tar.gz",
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRelease {
    pub tag: String,
    pub target: String,
    pub assets: Vec<String>,
    pub body: String,
    pub prerelease: bool,
    pub published: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait CommandRunner {
    fn run(
        &mut self,
        program: &str,
        args: &[String],
        dir: Option<&Path>,
    ) -> Result<CommandOutput, String>;
    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

pub struct ProcessRunner;
impl CommandRunner for ProcessRunner {
    fn run(
        &mut self,
        program: &str,
        args: &[String],
        dir: Option<&Path>,
    ) -> Result<CommandOutput, String> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(d) = dir {
            command.current_dir(d);
        }
        let output = command
            .output()
            .map_err(|e| format!("run {program}: {e}"))?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

fn arg(value: impl Into<String>) -> String {
    value.into()
}
fn successful(output: &CommandOutput, operation: &str) -> Result<(), String> {
    if output.code == Some(0) {
        Ok(())
    } else {
        Err(format!(
            "{operation} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn rest_release_from_json(json: &str) -> Result<DraftRelease, String> {
    let r: RestRelease = serde_json::from_str(json).map_err(|e| format!("decode release: {e}"))?;
    Ok(r.into_draft())
}

/// GitHub canonicalizes stored release bodies with one trailing newline, so
/// equality between the extracted notes and the stored body ignores only that
/// canonical trailing line ending.
fn notes_equivalent(stored: &str, notes: &str) -> bool {
    stored.trim_end_matches(['\n', '\r']) == notes.trim_end_matches(['\n', '\r'])
}

fn response_body(bytes: &[u8]) -> Result<&[u8], String> {
    if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
        Ok(&bytes[index + 4..])
    } else if let Some(index) = bytes.windows(2).position(|window| window == b"\n\n") {
        Ok(&bytes[index + 2..])
    } else {
        Ok(bytes)
    }
}

fn release_query<R: CommandRunner>(
    runner: &mut R,
    repo: &str,
    tag: &str,
) -> Result<Option<DraftRelease>, String> {
    let endpoint = format!("repos/{repo}/releases/tags/{tag}");
    let output = runner.run(
        "gh",
        &[
            arg("api"),
            arg(endpoint),
            arg("--include"),
            arg("--header"),
            arg("Accept: application/vnd.github+json"),
        ],
        None,
    )?;
    let text = String::from_utf8_lossy(&output.stdout);
    if output.code == Some(0) {
        return Ok(Some(rest_release_from_json(
            std::str::from_utf8(response_body(&output.stdout)?)
                .map_err(|_| "gh returned non-utf8")?,
        )?));
    }
    let status_404 = text
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 404 "))
        || String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line.contains("404"));
    if !status_404 {
        return Err(format!("release lookup failed: {}", text.trim()));
    }
    // Draft releases never resolve through the by-tag endpoint because the
    // tag is not a real ref until the draft is published. Fall back to the
    // paginated release list (raw REST: snake_case `tag_name`) and match on
    // the stored tag name. `--paginate` concatenates one JSON array document
    // per page into stdout, so `--include` (single-header framing) is not
    // used here; each page is decoded as a standalone array.
    let list = runner.run(
        "gh",
        &[
            arg("api"),
            arg(format!("repos/{repo}/releases")),
            arg("--paginate"),
            arg("--header"),
            arg("Accept: application/vnd.github+json"),
        ],
        None,
    )?;
    successful(&list, "release list")?;
    let body = std::str::from_utf8(&list.stdout).map_err(|_| "gh returned non-utf8")?;
    let stream = serde_json::Deserializer::from_str(body).into_iter::<Vec<RestRelease>>();
    let mut matches: Vec<DraftRelease> = Vec::new();
    let mut pages = 0usize;
    for page in stream {
        let page = page.map_err(|e| format!("decode release list page: {e}"))?;
        pages += 1;
        for release in page {
            if release.tag_name == tag {
                matches.push(release.into_draft());
            }
        }
    }
    if pages == 0 {
        return Err("release list returned no pages".into());
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err("multiple releases share this tag name; manual recovery required".into()),
    }
}

fn release_names() -> Vec<String> {
    archive_names()
        .iter()
        .map(|x| (*x).to_owned())
        .chain(std::iter::once("SHA256SUMS".to_owned()))
        .collect()
}
fn exact_membership(actual: &[String], expected: &[String]) -> bool {
    actual.len() == expected.len()
        && expected
            .iter()
            .all(|x| actual.iter().filter(|y| *y == x).count() == 1)
}
#[cfg(test)]
fn exact_names(actual: &[String]) -> bool {
    exact_membership(actual, &release_names())
}

#[allow(clippy::too_many_arguments)]
fn publish_release<R: CommandRunner>(
    runner: &mut R,
    root: &Path,
    tag: &str,
    expected_sha: &str,
    notes: &Path,
    notes_body: &str,
    assets: &[PathBuf],
    prerelease: bool,
) -> Result<(), String> {
    let head = runner.run("git", &[arg("rev-parse"), arg("HEAD")], Some(root))?;
    successful(&head, "git HEAD")?;
    if String::from_utf8_lossy(&head.stdout).trim() != expected_sha {
        return Err("checked-out HEAD does not match expected SHA".into());
    }
    let remote = runner.run(
        "git",
        &[arg("remote"), arg("get-url"), arg("origin")],
        Some(root),
    )?;
    successful(&remote, "git remote")?;
    let remote = String::from_utf8_lossy(&remote.stdout).trim().to_owned();
    if !remote.starts_with("https://") {
        return Err("origin remote must use HTTPS".into());
    }
    let refs = runner.run(
        "git",
        &[
            arg("ls-remote"),
            arg(&remote),
            arg(format!("refs/tags/{tag}")),
            arg(format!("refs/tags/{tag}^{{}}")),
        ],
        Some(root),
    )?;
    successful(&refs, "remote tag verification")?;
    let ref_text = String::from_utf8_lossy(&refs.stdout);
    let peeled = ref_text
        .lines()
        .find_map(|line| line.strip_suffix(&format!("\trefs/tags/{tag}^{{}}")))
        .unwrap_or("");
    if peeled != expected_sha {
        return Err("remote annotated tag does not peel to expected SHA".into());
    }
    let repo_out = runner.run(
        "gh",
        &[
            arg("repo"),
            arg("view"),
            arg("--json"),
            arg("nameWithOwner"),
            arg("-q"),
            arg(".nameWithOwner"),
        ],
        Some(root),
    )?;
    successful(&repo_out, "gh repository lookup")?;
    let repo = String::from_utf8_lossy(&repo_out.stdout).trim().to_owned();
    if repo.is_empty() {
        return Err("gh repository lookup returned empty".into());
    }
    let existing = release_query(runner, &repo, tag)?;
    let expected_names = release_names();
    if let Some(release) = existing {
        if release.published {
            return Err("release is already published; refusing overwrite".into());
        }
        if release.tag != tag
            || release.target != expected_sha
            || release.assets != expected_names
            || !notes_equivalent(&release.body, notes_body)
            || release.prerelease != prerelease
        {
            return Err(
                "draft tag/target/body/assets/prerelease mismatch; manual recovery required".into(),
            );
        }
    } else {
        let mut create = vec![
            arg("release"),
            arg("create"),
            arg(tag),
            arg("--verify-tag"),
            arg("--target"),
            arg(expected_sha),
            arg("--draft"),
            arg("--notes-file"),
            arg(notes.to_str().ok_or("non-utf8 notes")?),
        ];
        if prerelease {
            create.push(arg("--prerelease"));
        }
        create.extend(
            assets
                .iter()
                .map(|p| p.to_str().map(arg).ok_or("non-utf8 asset"))
                .collect::<Result<Vec<_>, _>>()?,
        );
        let sums = assets
            .first()
            .and_then(|p| p.parent())
            .map(|p| p.join("SHA256SUMS"))
            .ok_or("missing checksum manifest")?;
        create.push(arg(sums.to_str().ok_or("non-utf8 checksum manifest")?));
        let output = runner.run("gh", &create, Some(root))?;
        successful(&output, "gh release create")?;
        let created = release_query(runner, &repo, tag)?.ok_or("created release disappeared")?;
        if created.published
            || created.tag != tag
            || created.target != expected_sha
            || created.assets != expected_names
            || !notes_equivalent(&created.body, notes_body)
            || created.prerelease != prerelease
        {
            return Err("created draft metadata mismatch; manual recovery required".into());
        }
    }
    let download_dir = root
        .join("target")
        .join(format!(".xtask-download-{}", std::process::id()));
    if download_dir.exists() {
        return Err("download directory already exists; manual recovery required".into());
    }
    fs::create_dir_all(&download_dir).map_err(|e| format!("create download directory: {e}"))?;
    let result = (|| {
        let output = runner.run(
            "gh",
            &[
                arg("release"),
                arg("download"),
                arg(tag),
                arg("--dir"),
                arg(download_dir.to_str().ok_or("non-utf8 download directory")?),
            ],
            Some(root),
        )?;
        successful(&output, "gh release download")?;
        let downloaded = verify_asset_dir(&download_dir)?;
        let local_by_name: std::collections::HashMap<_, _> = assets
            .iter()
            .filter_map(|p| p.file_name().map(|n| (n.to_owned(), p)))
            .collect();
        for fetched in downloaded
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| n != "SHA256SUMS"))
        {
            let name = fetched.file_name().ok_or("downloaded asset has no name")?;
            let local = local_by_name
                .get(name)
                .ok_or("downloaded asset name is not local")?;
            if sha256(local)? != sha256(fetched)? {
                return Err(format!(
                    "downloaded asset checksum differs for {}",
                    name.to_string_lossy()
                ));
            }
        }
        let output = runner.run(
            "gh",
            &[arg("release"), arg("edit"), arg(tag), arg("--draft=false")],
            Some(root),
        )?;
        successful(&output, "gh release publish")
    })();
    let _ = fs::remove_dir_all(&download_dir);
    result
}

pub fn verify_checksums(checksums: &str, expected: &[&str]) -> Result<(), String> {
    let mut seen = Vec::new();
    for line in checksums.lines() {
        let mut p = line.split_whitespace();
        let sum = p.next();
        let file = p.next().map(|x| x.trim_start_matches('*'));
        if sum
            .map(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
            .unwrap_or(false)
        {
            if let Some(f) = file {
                seen.push(f.to_owned());
            }
        } else {
            return Err("invalid checksum line".into());
        }
    }
    if seen.len() != expected.len()
        || expected.iter().any(|x| !seen.iter().any(|y| y == x))
        || seen.iter().any(|x| !expected.iter().any(|y| y == x))
    {
        return Err("checksums must contain exactly the expected archives".into());
    }
    Ok(())
}

fn command(
    program: &str,
    args: &[&str],
    dir: Option<&Path>,
) -> Result<std::process::Output, String> {
    let mut c = Command::new(program);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = dir {
        c.current_dir(d);
    }
    let out = c.output().map_err(|e| format!("run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out)
}
fn bounded_stdio_smoke(root: &Path, binary: &str) -> Result<(), String> {
    let mut child = Command::new(binary)
        .current_dir(root)
        .env("CARGO_PKG_VERSION", "9.9.9-conflicting-runtime-value")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {binary}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("stdio stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("stdio stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("stdio stderr unavailable")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        let result = (|| -> Result<Vec<serde_json::Value>, String> {
            let mut values = Vec::new();
            for _ in 0..2 {
                let line = lines
                    .next()
                    .ok_or("stdio ended before response")?
                    .map_err(|e| format!("read stdio: {e}"))?;
                if line.trim().is_empty() {
                    return Err("blank stdio line".into());
                }
                values.push(
                    serde_json::from_str(&line)
                        .map_err(|e| format!("invalid protocol JSON: {e}"))?,
                );
            }
            Ok(values)
        })();
        let _ = tx.send(result);
    });
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"native-smoke","version":"1"}}}"#;
    let tools = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    writeln!(stdin, "{init}\n{tools}\n{list}").map_err(|e| format!("write stdio: {e}"))?;
    drop(stdin);
    let mut child = child;
    let values = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("stdio smoke deadline exceeded".into());
        }
    };
    if values
        .iter()
        .any(|v| v.get("jsonrpc") != Some(&serde_json::Value::String("2.0".into())))
    {
        return Err("protocol response missing jsonrpc 2.0".into());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("wait stdio: {e}"))? {
            if !status.success() {
                return Err("bounded production stdio smoke failed".into());
            }
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("stdio shutdown deadline exceeded".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    let mut stderr_reader = BufReader::new(stderr);
    let mut stderr_text = String::new();
    let _ = stderr_reader.read_to_string(&mut stderr_text);
    if !stderr_text.is_empty() {
        return Err("stdio smoke wrote stderr".into());
    }
    Ok(())
}

fn verify_binary(root: &Path, binary: &Path, version: &str) -> Result<(), String> {
    if !binary.is_file() {
        return Err(format!("binary does not exist: {}", binary.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(binary)
            .map_err(|e| e.to_string())?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err("binary is not executable".into());
        }
    }
    let binary = binary.to_str().ok_or("non-utf8 binary")?;
    for flag in ["--version", "-V", "--help"] {
        let out = Command::new(binary)
            .arg(flag)
            .current_dir(root)
            .env("CARGO_PKG_VERSION", "9.9.9-conflicting-runtime-value")
            .output()
            .map_err(|e| format!("run {binary}: {e}"))?;
        if !out.status.success() || !out.stderr.is_empty() {
            return Err(format!("{flag} failed or wrote stderr"));
        }
        if flag != "--help" && out.stdout != format!("sqlite-mcp {version}\n").as_bytes() {
            return Err(format!("{flag} output mismatch"));
        }
    }
    bounded_stdio_smoke(root, binary)
}

fn package_binary(
    root: &Path,
    tag: &str,
    target: &str,
    binary: &Path,
    outdir: &Path,
) -> Result<PathBuf, String> {
    let host = String::from_utf8(command("rustc", &["-vV"], Some(root))?.stdout)
        .map_err(|_| "rustc output not utf8")?
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .ok_or("rustc host missing")?
        .to_owned();
    if target != host {
        return Err(format!("target {target} does not match native host {host}"));
    }
    verify_binary(root, binary, &validate_repository(root, tag)?.version)?;
    fs::create_dir_all(outdir).map_err(|e| e.to_string())?;
    let archive = outdir.join(format!("sqlite-mcp-{target}.tar.gz"));
    if archive.exists() {
        return Err(format!(
            "refusing to overwrite existing archive {}",
            archive.display()
        ));
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let staging = outdir.join(format!(".stage-{target}-{stamp}"));
    fs::create_dir(&staging).map_err(|e| e.to_string())?;
    fs::copy(binary, staging.join("sqlite-mcp")).map_err(|e| e.to_string())?;
    let result = (|| {
        command(
            "tar",
            &[
                "-czf",
                archive.to_str().ok_or("non-utf8 output")?,
                "-C",
                staging.to_str().ok_or("non-utf8 staging")?,
                "sqlite-mcp",
            ],
            Some(root),
        )?;
        let list = command("tar", &["-tzf", archive.to_str().unwrap()], Some(root))?;
        if String::from_utf8_lossy(&list.stdout)
            .lines()
            .collect::<Vec<_>>()
            != ["sqlite-mcp"]
        {
            return Err("archive must contain only sqlite-mcp".into());
        }
        let extract = outdir.join(format!(".extract-{target}-{stamp}"));
        fs::create_dir(&extract).map_err(|e| e.to_string())?;
        command(
            "tar",
            &[
                "-xzf",
                archive.to_str().unwrap(),
                "-C",
                extract.to_str().unwrap(),
            ],
            Some(root),
        )?;
        let extracted = extract.join("sqlite-mcp");
        verify_binary(root, &extracted, &validate_repository(root, tag)?.version)?;
        let _ = fs::remove_dir_all(extract);
        Ok(archive.clone())
    })();
    let _ = fs::remove_dir_all(staging);
    result
}
/// Raw REST (`gh api`) release representation: GitHub's REST payloads use
/// snake_case fields (`tag_name`, `target_commitish`, `published_at`). This
/// is the ONLY valid shape for the raw `gh api` by-tag lookup and the
/// paginated release list. The camelCase field spellings (`tagName`,
/// `isDraft`, `targetCommitish`) belong exclusively to the separate
/// `gh release view --json` output shape and never appear in REST payloads.
#[derive(Deserialize)]
struct RestRelease {
    #[serde(default)]
    draft: bool,
    prerelease: bool,
    tag_name: String,
    target_commitish: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<RestAsset>,
}
impl RestRelease {
    fn into_draft(self) -> DraftRelease {
        DraftRelease {
            tag: self.tag_name,
            target: self.target_commitish,
            assets: self.assets.into_iter().map(|a| a.name).collect(),
            body: self.body,
            prerelease: self.prerelease,
            published: !self.draft,
        }
    }
}
#[derive(Deserialize)]
struct RestAsset {
    name: String,
}
fn sha256(path: &Path) -> Result<String, String> {
    let p = path.to_str().ok_or("non-utf8 asset")?;
    let o = if cfg!(target_os = "macos") {
        command("shasum", &["-a", "256", p], None)?
    } else {
        command("sha256sum", &[p], None)?
    };
    Ok(String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .next()
        .ok_or("missing checksum")?
        .to_string())
}
fn verify_asset_dir(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let expected = archive_names();
    let mut actual = fs::read_dir(dir)
        .map_err(|e| format!("read asset directory: {e}"))?
        .map(|entry| entry.map(|e| e.file_name()).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    actual.sort();
    let actual_names: Vec<String> = actual
        .iter()
        .filter_map(|x| x.to_str().map(str::to_owned))
        .filter(|x| x != "SHA256SUMS")
        .collect();
    let archive_expected: Vec<String> = expected.iter().map(|x| (*x).to_owned()).collect();
    if !exact_membership(&actual_names, &archive_expected) {
        return Err("asset directory must contain exactly the four archives".into());
    }
    let mut expected_files: Vec<_> = expected.iter().map(std::ffi::OsString::from).collect();
    expected_files.push(std::ffi::OsString::from("SHA256SUMS"));
    expected_files.sort();
    if actual != expected_files {
        return Err("asset directory must contain exactly the four archives and SHA256SUMS".into());
    }
    let mut assets = Vec::new();
    for n in expected {
        let p = dir.join(n);
        if !p.is_file() {
            return Err(format!("missing asset {n}"));
        };
        assets.push(p);
    }
    let sums = dir.join("SHA256SUMS");
    let text = fs::read_to_string(&sums).map_err(|e| format!("read checksums: {e}"))?;
    verify_checksums(&text, &expected)?;
    for p in &assets {
        let want = text
            .lines()
            .find(|l| l.ends_with(p.file_name().unwrap().to_str().unwrap()))
            .and_then(|l| l.split_whitespace().next())
            .unwrap();
        if sha256(p)? != want {
            return Err(format!("checksum mismatch for {}", p.display()));
        }
    }
    Ok(assets)
}

const PUBLIC_REPO: &str = "tilmon-engineering/sqlite-mcp";
const PUBLIC_API: &str = "https://api.github.com";

fn valid_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Execute one anonymous, bounded curl request, writing the response body to `dest`.
/// Only transient GitHub visibility statuses are retried (at most five attempts).
pub fn anonymous_http_get<R: CommandRunner>(
    runner: &mut R,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    if !url.starts_with("https://") || url.contains(" ") {
        return Err("public URL must be an HTTPS URL".into());
    }
    let output_path = dest.to_str().ok_or("non-utf8 HTTP output path")?;
    let mut last = String::new();
    for attempt in 0..5 {
        let args = vec![
            arg("--disable"),
            arg("--proto"),
            arg("=https"),
            arg("--proto-redir"),
            arg("=https"),
            arg("--location"),
            arg("--silent"),
            arg("--show-error"),
            arg("--connect-timeout"),
            arg("10"),
            arg("--max-time"),
            arg("30"),
            arg("--output"),
            arg(output_path),
            arg("--write-out"),
            arg("%{http_code}"),
            arg(url),
        ];
        let output = match runner.run("curl", &args, None) {
            Ok(output) => output,
            Err(error) => {
                let _ = fs::remove_file(dest);
                return Err(format!("anonymous HTTP request failed: {error}"));
            }
        };
        let status = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.code == Some(0) && status.len() == 3 && status.starts_with('2') {
            return Ok(());
        }
        let retryable = matches!(status.as_str(), "404" | "502" | "503" | "504");
        last = if status.is_empty() {
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        } else {
            format!("HTTP {status}")
        };
        let _ = fs::remove_file(dest);
        if !retryable || attempt == 4 {
            return Err(format!("anonymous HTTP request failed: {last}"));
        }
        runner.sleep(Duration::from_secs(2));
    }
    Err(format!("anonymous HTTP request failed: {last}"))
}

pub fn fetch_public_assets<R: CommandRunner>(
    runner: &mut R,
    tag: &str,
    dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let version = tag.strip_prefix('v').ok_or("invalid release tag")?;
    validate_tag(tag, version)?;
    if dir.exists()
        && fs::read_dir(dir)
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
    {
        return Err("asset destination must be empty".into());
    }
    fs::create_dir_all(dir).map_err(|e| format!("create asset destination: {e}"))?;
    let result = (|| {
        for name in release_names() {
            let url = format!("https://github.com/{PUBLIC_REPO}/releases/download/{tag}/{name}");
            anonymous_http_get(runner, &url, &dir.join(&name))?;
        }
        verify_asset_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(dir);
    }
    result
}

pub fn verify_downloads(root: &Path, tag: &str, dir: &Path, binary: &Path) -> Result<(), String> {
    let meta = validate_repository(root, tag)?;
    let _assets = verify_asset_dir(dir)?;
    verify_binary(root, binary, &meta.version)
}

#[derive(Debug, Serialize)]
struct PublicEvidence {
    tag: String,
    expected_sha: String,
    release_url: String,
    asset_urls: Vec<String>,
    assertions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PublicRelease {
    #[serde(rename = "tag_name")]
    tag: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
    body: String,
    assets: Vec<PublicAsset>,
}
#[derive(Debug, Deserialize)]
struct PublicAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}
#[derive(Debug, Deserialize)]
struct GitRef {
    object: GitObject,
}
#[derive(Debug, Deserialize)]
struct AnnotatedTag {
    #[serde(rename = "tag")]
    name: String,
    object: GitObject,
}
#[derive(Debug, Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

pub fn verify_public_release<R: CommandRunner>(
    runner: &mut R,
    root: &Path,
    tag: &str,
    expected_sha: &str,
    evidence: &Path,
) -> Result<(), String> {
    let meta = validate_repository(root, tag)?;
    if !valid_sha(expected_sha) {
        return Err("EXPECTED_SHA must be a full hexadecimal commit SHA".into());
    }
    let temp = root.join("target").join(format!(
        ".xtask-public-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("clock before Unix epoch: {e}"))?
            .as_nanos()
    ));
    if temp.exists() {
        return Err("public verification temporary directory exists".into());
    }
    fs::create_dir_all(&temp).map_err(|e| e.to_string())?;
    let result = (|| {
        let get = |runner: &mut R, path: &str, file: &str| -> Result<Vec<u8>, String> {
            let p = temp.join(file);
            anonymous_http_get(runner, &format!("{PUBLIC_API}{path}"), &p)?;
            fs::read(p).map_err(|e| e.to_string())
        };
        let release: PublicRelease = serde_json::from_slice(&get(
            runner,
            &format!("/repos/{PUBLIC_REPO}/releases/tags/{tag}"),
            "release.json",
        )?)
        .map_err(|e| format!("decode public release: {e}"))?;
        if release.tag != tag
            || release.draft
            || release.prerelease
            || release.published_at.is_none()
            || !notes_equivalent(&release.body, &release_notes(root, tag)?)
        {
            return Err("public release metadata mismatch".into());
        }
        let expected = release_names();
        if release.assets.len() != expected.len()
            || release.assets.iter().any(|a| {
                a.size == 0
                    || !expected.contains(&a.name)
                    || a.browser_download_url
                        != format!(
                            "https://github.com/{PUBLIC_REPO}/releases/download/{tag}/{}",
                            a.name
                        )
            })
            || !exact_membership(
                &release
                    .assets
                    .iter()
                    .map(|a| a.name.clone())
                    .collect::<Vec<_>>(),
                &expected,
            )
        {
            return Err("public release assets mismatch".into());
        }
        let reference: GitRef = serde_json::from_slice(&get(
            runner,
            &format!("/repos/{PUBLIC_REPO}/git/ref/tags/{tag}"),
            "ref.json",
        )?)
        .map_err(|e| format!("decode tag ref: {e}"))?;
        if reference.object.kind != "tag" {
            return Err("tag is not annotated".into());
        }
        let annotated: AnnotatedTag = serde_json::from_slice(&get(
            runner,
            &format!("/repos/{PUBLIC_REPO}/git/tags/{}", reference.object.sha),
            "tag.json",
        )?)
        .map_err(|e| format!("decode annotated tag: {e}"))?;
        if annotated.name != tag {
            return Err("annotated tag name mismatch".into());
        }
        if annotated.object.kind != "commit" || annotated.object.sha != expected_sha {
            return Err("annotated tag does not resolve to expected SHA".into());
        }
        let asset_urls = release
            .assets
            .iter()
            .map(|a| a.browser_download_url.clone())
            .collect();
        let out = PublicEvidence {
            tag: tag.into(),
            expected_sha: expected_sha.into(),
            release_url: format!("https://github.com/{PUBLIC_REPO}/releases/tag/{tag}"),
            asset_urls,
            assertions: vec![
                "release metadata".into(),
                "annotated tag peel".into(),
                "exact assets".into(),
                format!("workspace version {}", meta.version),
            ],
        };
        let text = serde_json::to_vec_pretty(&out).map_err(|e| e.to_string())?;
        fs::write(evidence, text).map_err(|e| format!("write evidence: {e}"))
    })();
    let _ = fs::remove_dir_all(temp);
    result
}
fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let action = args.next().ok_or(
        "usage: xtask <release-check|release-notes|expected-tag|native-smoke|package|publish|fetch-public-assets|verify-downloads|verify-public-release> ...",
    )?;
    let root = env::current_dir().map_err(|e| e.to_string())?;
    match action.as_str() {
        "release-check" => {
            let tag = args.next().ok_or("release-check requires TAG")?;
            validate_repository(&root, &tag)?;
            println!("release metadata valid: {tag}");
        }
        "release-notes" => {
            let tag = args.next().ok_or("release-notes requires TAG")?;
            let out = PathBuf::from(args.next().ok_or("release-notes requires OUTPUT_PATH")?);
            let notes = release_notes(&root, &tag)?;
            fs::write(out, notes).map_err(|e| format!("write release notes: {e}"))?;
        }
        "expected-tag" => {
            if args.next().is_some() {
                return Err("expected-tag takes no arguments".into());
            }
            let root_manifest: toml::Value = toml::from_str(
                &fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            let version = root_manifest
                .get("workspace")
                .and_then(|x| x.get("package"))
                .and_then(|x| x.get("version"))
                .and_then(|x| x.as_str())
                .ok_or("missing workspace version")?;
            println!("{}", expected_tag(version));
        }
        "native-smoke" => {
            let tag = args.next().ok_or("native-smoke requires TAG")?;
            let meta = validate_repository(&root, &tag)?;
            let binary = PathBuf::from(args.next().ok_or("native-smoke requires BINARY_PATH")?);
            verify_binary(&root, &binary, &meta.version)?;
            println!(
                "native release smoke passed for {tag} ({})",
                binary.display()
            );
        }
        "package" => {
            let tag = args.next().ok_or("package requires TAG")?;
            let target = args.next().ok_or("package requires TARGET")?;
            let binary = PathBuf::from(args.next().ok_or("package requires BINARY")?);
            let outdir = PathBuf::from(args.next().ok_or("package requires OUTPUT_DIR")?);
            let meta = validate_repository(&root, &tag)?;
            verify_binary(&root, &binary, &meta.version)?;
            let archive = package_binary(&root, &tag, &target, &binary, &outdir)?;
            println!("{}", archive.display());
        }
        "fetch-public-assets" => {
            let tag = args.next().ok_or("fetch-public-assets requires TAG")?;
            let dir = PathBuf::from(
                args.next()
                    .ok_or("fetch-public-assets requires ASSET_DIR")?,
            );
            let mut runner = ProcessRunner;
            fetch_public_assets(&mut runner, &tag, &dir)?;
            println!("fetched public assets for {tag}");
        }
        "verify-downloads" => {
            let tag = args.next().ok_or("verify-downloads requires TAG")?;
            let dir = PathBuf::from(args.next().ok_or("verify-downloads requires ASSET_DIR")?);
            let binary = PathBuf::from(args.next().ok_or("verify-downloads requires BINARY_PATH")?);
            verify_downloads(&root, &tag, &dir, &binary)?;
            println!("download verification passed for {tag}");
        }
        "verify-public-release" => {
            let tag = args.next().ok_or("verify-public-release requires TAG")?;
            let sha = args
                .next()
                .ok_or("verify-public-release requires EXPECTED_SHA")?;
            let evidence = PathBuf::from(
                args.next()
                    .ok_or("verify-public-release requires EVIDENCE_PATH")?,
            );
            let mut runner = ProcessRunner;
            verify_public_release(&mut runner, &root, &tag, &sha, &evidence)?;
            println!("public release verification passed for {tag}");
        }
        "publish" => {
            let tag = args.next().ok_or("publish requires TAG")?;
            let expected_sha = args.next().ok_or("publish requires EXPECTED_SHA")?;
            let dir = PathBuf::from(args.next().ok_or("publish requires ASSET_DIR")?);
            let meta = validate_repository(&root, &tag)?;
            let notes = release_notes(&root, &tag)?;
            let notes_path = root.join("target").join("xtask-release-notes.md");
            fs::create_dir_all(notes_path.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::write(&notes_path, &notes).map_err(|e| e.to_string())?;
            let sums = dir.join("SHA256SUMS");
            if !sums.exists() {
                let mut body = String::new();
                for name in archive_names() {
                    let path = dir.join(name);
                    if !path.is_file() {
                        return Err(format!("missing asset {name}"));
                    }
                    body.push_str(&format!("{}  {name}\n", sha256(&path)?));
                }
                fs::write(&sums, body).map_err(|e| format!("write checksums: {e}"))?;
            }
            let assets = verify_asset_dir(&dir)?;
            let mut runner = ProcessRunner;
            publish_release(
                &mut runner,
                &root,
                &tag,
                &expected_sha,
                &notes_path,
                &notes,
                &assets,
                meta.version.contains('-'),
            )?;
            println!("published {tag}");
        }
        _ => return Err(format!("unknown command {action}")),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expected_tag_preserves_semver() {
        assert_eq!(expected_tag("1.2.3-beta.1+build"), "v1.2.3-beta.1+build");
    }
    #[test]
    fn notes_exact_section() {
        let c =
            "# Changelog\n\n## [0.1.0] - 2026-01-02\n### Added\n- x\n\n## [Unreleased]\n- next\n";
        assert_eq!(extract_notes(c, "0.1.0").unwrap(), "### Added\n- x\n");
    }
    #[test]
    fn notes_fenced_headings() {
        let c =
            "# Changelog\n\n```\n## [0.1.0] - 2020-01-01\n```\n## [0.1.0] - 2026-01-02\n- real\n";
        assert_eq!(extract_notes(c, "0.1.0").unwrap(), "- real\n");
    }
    #[test]
    fn notes_reject_invalid_sections() {
        assert!(extract_notes("# Changelog\n## [0.1.0] - nope\n- x\n", "0.1.0").is_err());
        assert!(
            extract_notes(
                "# Changelog\n## [0.1.0] - 2020-01-01\n\n## [0.1.0] - 2020-01-02\n- x\n",
                "0.1.0"
            )
            .is_err()
        );
    }
    #[test]
    fn checksum_exact_membership() {
        let a = archive_names();
        let good = a
            .iter()
            .enumerate()
            .map(|(i, name)| format!("{:064x}  {name}\n", i + 1))
            .collect::<String>();
        assert!(verify_checksums(&good, &a).is_ok());
        assert!(verify_checksums(&format!("{:064x}  {}\n", 1, a[0]), &a).is_err());
    }
    #[test]
    fn release_rejects_literal_version_without_inheritance() {
        let dir = tempfile_dir();
        fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers=[\"crates/sqlite-mcp\",\"crates/sqlite-mcp-core\"]\n[workspace.package]\nversion=\"0.1.0\"\n").unwrap();
        fs::write(dir.join("Cargo.lock"), "[[package]]\nname = \"sqlite-mcp\"\nversion = \"0.1.0\"\n[[package]]\nname = \"sqlite-mcp-core\"\nversion = \"0.1.0\"\n").unwrap();
        for p in ["sqlite-mcp", "sqlite-mcp-core"] {
            let d = dir.join("crates").join(p);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("Cargo.toml"), "[package]\nversion=\"0.1.0\"\n").unwrap();
        }
        assert!(validate_repository(&dir, "v0.1.0").is_err());
    }
    fn tempfile_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = env::temp_dir().join(format!(
            "xtask-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
    #[derive(Default)]
    struct FakeRunner {
        calls: Vec<(String, Vec<String>)>,
        release: Option<String>,
        fail_query: Option<String>,
        fail_download: bool,
        fail_create: bool,
        corrupt_download: bool,
        postcreate_release: Option<String>,
        extra_release: Option<String>,
        publish_called: bool,
        public_responses: Vec<String>,
        public_http_failure: bool,
        /// Queued raw REST release-list pages (snake_case payloads). Served
        /// concatenated for the paginated list endpoint only; an empty queue
        /// falls back to `release`/`extra_release`.
        rest_release_pages: Vec<String>,
    }
    impl CommandRunner for FakeRunner {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
            _: Option<&Path>,
        ) -> Result<CommandOutput, String> {
            self.calls.push((program.to_owned(), args.to_vec()));
            if program == "git" && args.first().map(String::as_str) == Some("rev-parse") {
                return Ok(output(0, "sha\n", ""));
            }
            if program == "git" && args.first().map(String::as_str) == Some("remote") {
                return Ok(output(0, "https://github.com/o/r.git\n", ""));
            }
            if program == "git" && args.first().map(String::as_str) == Some("ls-remote") {
                return Ok(output(
                    0,
                    "tag\trefs/tags/v1.0.0\nsha\trefs/tags/v1.0.0^{}\n",
                    "",
                ));
            }
            if program == "gh" && args.starts_with(&[arg("repo"), arg("view")]) {
                return Ok(output(0, "o/r\n", ""));
            }
            if program == "curl" {
                let output_path = args
                    .windows(2)
                    .find(|pair| pair[0] == "--output")
                    .map(|pair| PathBuf::from(&pair[1]))
                    .ok_or_else(|| "fake curl missing output path".to_string())?;
                if self.public_http_failure {
                    return Ok(output(1, "", "HTTP/1.1 500 Internal Server Error"));
                }
                let body = self.public_responses.first().cloned().unwrap_or_default();
                if !self.public_responses.is_empty() {
                    self.public_responses.remove(0);
                }
                fs::write(output_path, body).map_err(|e| e.to_string())?;
                return Ok(output(0, "200", ""));
            }
            if program == "gh" && args.first().map(String::as_str) == Some("api") {
                if let Some(error) = &self.fail_query {
                    return Ok(output(1, "", error));
                }
                // Model real GitHub: draft releases never resolve through the
                // by-tag endpoint, while the release list contains them. The
                // by-tag call uses `--include` (single-header framing); the
                // paginated list call emits raw concatenated JSON pages.
                let by_tag = args
                    .get(1)
                    .map(String::as_str)
                    .is_some_and(|endpoint| endpoint.contains("/releases/tags/"));
                let ok = |body: &str| output(0, &format!("HTTP/1.1 200 OK\r\n\r\n{body}"), "");
                if !by_tag && !self.rest_release_pages.is_empty() {
                    let body = self.rest_release_pages.concat();
                    self.rest_release_pages.clear();
                    return Ok(output(0, &body, ""));
                }
                return match (&self.release, by_tag) {
                    (Some(release), true) if release.contains("\"draft\":true") => {
                        Ok(output(1, "", "HTTP/1.1 404 Not Found"))
                    }
                    (Some(release), true) => Ok(ok(release)),
                    (Some(release), false) => match &self.extra_release {
                        Some(extra) => Ok(output(0, &format!("[{release},{extra}]"), "")),
                        None => Ok(output(0, &format!("[{release}]"), "")),
                    },
                    (None, true) => Ok(output(1, "", "HTTP/1.1 404 Not Found")),
                    (None, false) => Ok(output(0, "[]", "")),
                };
            }
            if program == "gh" && args.get(1).map(String::as_str) == Some("create") {
                if self.fail_create {
                    return Ok(output(1, "", "already exists"));
                }
                // Model GitHub storing the created draft: the post-create
                // lookup must find it through the list endpoint.
                self.release = self
                    .postcreate_release
                    .take()
                    .or_else(|| Some(release_json(true, "sha", "notes")));
                return Ok(output(0, "", ""));
            }
            if program == "gh" && args.get(1).map(String::as_str) == Some("download") {
                if self.fail_download {
                    return Ok(output(1, "", "download failed"));
                }
                let dir = PathBuf::from(
                    args.iter()
                        .find(|x| x.as_str() == "--dir")
                        .and_then(|_| args.last())
                        .unwrap(),
                );
                fs::create_dir_all(&dir).unwrap();
                let source = PathBuf::from("SOURCE");
                for name in archive_names() {
                    fs::write(
                        dir.join(name),
                        if self.corrupt_download {
                            b"bad".as_slice()
                        } else {
                            b"same".as_slice()
                        },
                    )
                    .unwrap();
                }
                let mut sums = String::new();
                for name in archive_names() {
                    let checksum = sha256(&dir.join(name)).unwrap();
                    sums.push_str(&format!("{checksum}  {name}\n"));
                }
                fs::write(dir.join("SHA256SUMS"), sums).unwrap();
                let _ = source;
                return Ok(output(0, "", ""));
            }
            if program == "gh" && args.get(1).map(String::as_str) == Some("edit") {
                self.publish_called = true;
                return Ok(output(0, "", ""));
            }
            Err(format!("unexpected command {program} {args:?}"))
        }
    }
    fn output(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }
    fn release_json(draft: bool, target: &str, body: &str) -> String {
        let assets = archive_names()
            .iter()
            .map(|name| format!(r#"{{"name":"{name}","size":1}}"#))
            .chain(std::iter::once(
                r#"{"name":"SHA256SUMS","size":1}"#.to_owned(),
            ))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"draft":{draft},"prerelease":false,"tag_name":"v1.0.0","target_commitish":"{target}","body":"{body}","assets":[{assets}]}}"#
        )
    }
    fn run_publish(
        fake: &mut FakeRunner,
        release: Option<String>,
        root: &Path,
    ) -> Result<(), String> {
        fake.release = release;
        let notes = root.join("notes");
        fs::write(&notes, "notes").unwrap();
        let assets = local_assets(root);
        publish_release(fake, root, "v1.0.0", "sha", &notes, "notes", &assets, false)
    }
    #[test]
    fn publish_rejects_published_release() {
        let root = tempfile_dir();
        let mut fake = FakeRunner::default();
        let result = run_publish(&mut fake, Some(release_json(false, "sha", "notes")), &root);
        assert!(result.is_err());
        assert!(
            !fake
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("create"))
        );
        assert!(
            !fake
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("download"))
        );
        assert!(!fake.publish_called);
    }
    #[test]
    fn publish_draft_mismatch_requires_recovery() {
        let root = tempfile_dir();
        let mut fake = FakeRunner::default();
        let result = run_publish(&mut fake, Some(release_json(true, "other", "notes")), &root);
        assert!(result.is_err());
        assert!(
            !fake
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("create"))
        );
        assert!(
            !fake
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("download"))
        );
        assert!(!fake.publish_called);
    }
    #[test]
    fn publish_matching_draft_resume() {
        let root = tempfile_dir();
        let mut fake = FakeRunner::default();
        let result = run_publish(&mut fake, Some(release_json(true, "sha", "notes")), &root);
        assert!(result.is_ok(), "{result:?}");
        assert!(
            !fake
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("create"))
        );
        assert!(
            fake.calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("download"))
        );
        assert!(fake.publish_called);
    }
    #[test]
    fn publish_resume_finds_draft_via_list_not_tag() {
        // Real GitHub: an existing draft is invisible to the by-tag lookup
        // and must be discovered through the release list.
        let root = tempfile_dir();
        let mut fake = FakeRunner::default();
        let result = run_publish(&mut fake, Some(release_json(true, "sha", "notes")), &root);
        assert!(result.is_ok(), "{result:?}");
        assert!(
            !fake
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("create"))
        );
        assert!(fake.calls.iter().any(|(p, a)| {
            p == "gh"
                && a.first().map(String::as_str) == Some("api")
                && a.get(1)
                    .map(String::as_str)
                    .is_some_and(|e| e.ends_with("/releases"))
        }));
        assert!(fake.publish_called);
    }
    #[test]
    fn publish_multiple_matching_drafts_require_recovery() {
        let root = tempfile_dir();
        let mut fake = FakeRunner {
            release: Some(release_json(true, "sha", "notes")),
            extra_release: Some(release_json(true, "sha", "notes")),
            ..Default::default()
        };
        let release = fake.release.clone();
        let result = run_publish(&mut fake, release, &root);
        assert!(result.is_err());
        assert!(!fake.publish_called);
    }

    fn rest_page(tag: &str, draft: bool) -> String {
        format!(
            r#"[{{"draft":{draft},"prerelease":false,"tag_name":"{tag}","target_commitish":"sha","body":"notes","assets":[]}}]"#
        )
    }

    fn assert_paginated_list_call(fake: &FakeRunner) {
        let list_call = fake
            .calls
            .iter()
            .find(|(program, args)| {
                program == "gh"
                    && args.first().map(String::as_str) == Some("api")
                    && args
                        .get(1)
                        .map(String::as_str)
                        .is_some_and(|endpoint| endpoint.ends_with("/releases"))
            })
            .expect("paginated release-list call recorded");
        assert!(
            list_call.1.iter().any(|a| a == "--paginate"),
            "release-list call must use --paginate: {:?}",
            list_call.1
        );
        assert!(
            !list_call.1.iter().any(|a| a == "--include"),
            "release-list call must not use --include (multi-page framing): {:?}",
            list_call.1
        );
        assert!(
            fake.rest_release_pages.is_empty(),
            "all queued REST pages must be consumed"
        );
    }

    #[test]
    fn rest_list_page_two_only_match() {
        // The matching draft lives on the second page; the first page holds
        // an unrelated tag. Discovery must survive page concatenation.
        let mut fake = FakeRunner {
            rest_release_pages: vec![rest_page("v0.9.0", false), rest_page("v1.0.0", true)],
            ..Default::default()
        };
        let found = release_query(&mut fake, "o/r", "v1.0.0").expect("query ok");
        let found = found.expect("draft discovered on page two");
        assert_eq!(found.tag, "v1.0.0");
        assert!(!found.published, "draft must be reported as unpublished");
        assert_paginated_list_call(&fake);
    }

    #[test]
    fn rest_list_malformed_page_is_an_error() {
        let mut fake = FakeRunner {
            rest_release_pages: vec![rest_page("v0.9.0", false), "{\"unterminated".to_owned()],
            ..Default::default()
        };
        let error = release_query(&mut fake, "o/r", "v1.0.0").expect_err("malformed page");
        assert!(
            error.contains("decode release list page"),
            "unexpected error: {error}"
        );
        assert_paginated_list_call(&fake);
    }

    #[test]
    fn rest_list_non_array_page_is_an_error() {
        let mut fake = FakeRunner {
            rest_release_pages: vec![r#"{"tag_name":"v1.0.0","draft":true}"#.to_owned()],
            ..Default::default()
        };
        let error = release_query(&mut fake, "o/r", "v1.0.0").expect_err("non-array page");
        assert!(
            error.contains("decode release list page"),
            "unexpected error: {error}"
        );
        assert_paginated_list_call(&fake);
    }

    #[test]
    fn rest_list_duplicate_across_pages_is_ambiguity() {
        let mut fake = FakeRunner {
            rest_release_pages: vec![rest_page("v1.0.0", true), rest_page("v1.0.0", true)],
            ..Default::default()
        };
        let error = release_query(&mut fake, "o/r", "v1.0.0").expect_err("duplicates");
        assert!(
            error.contains("multiple releases share this tag name"),
            "unexpected error: {error}"
        );
        assert_paginated_list_call(&fake);
    }

    #[test]
    fn publish_duplicate_drafts_across_rest_pages_require_recovery() {
        let root = tempfile_dir();
        let mut fake = FakeRunner {
            rest_release_pages: vec![rest_page("v1.0.0", true), rest_page("v1.0.0", true)],
            ..Default::default()
        };
        let result = run_publish(&mut fake, None, &root);
        assert!(result.is_err());
        assert!(!fake.publish_called);
        assert_paginated_list_call(&fake);
    }
    #[test]
    fn publish_checksum_exact_membership() {
        let names = release_names();
        assert!(exact_names(&names));
        assert!(!exact_names(&[names[0].clone(), names[0].clone()]));
    }
    #[test]
    fn publish_absent_and_auth_are_distinguished() {
        let root = tempfile_dir();
        let mut absent = FakeRunner::default();
        assert!(run_publish(&mut absent, None, &root).is_ok());
        assert!(
            absent
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("create"))
        );
        let root = tempfile_dir();
        let mut auth = FakeRunner {
            fail_query: Some("HTTP/1.1 401 Unauthorized".into()),
            ..Default::default()
        };
        assert!(run_publish(&mut auth, None, &root).is_err());
        assert!(
            !auth
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("create"))
        );
        assert!(
            !auth
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("download"))
        );
        assert!(!auth.publish_called);
    }
    fn local_assets(root: &Path) -> Vec<PathBuf> {
        let dir = root.join("local");
        fs::create_dir_all(&dir).unwrap();
        let mut sums = String::new();
        for name in archive_names() {
            fs::write(dir.join(name), b"same").unwrap();
            sums.push_str(&format!("{}  {name}\n", sha256(&dir.join(name)).unwrap()));
        }
        fs::write(dir.join("SHA256SUMS"), sums).unwrap();
        archive_names().iter().map(|n| dir.join(n)).collect()
    }
    #[test]
    fn publish_new_release_executes_full_success_path() {
        let root = tempfile_dir();
        let notes = root.join("notes");
        fs::write(&notes, "notes").unwrap();
        let assets = local_assets(&root);
        let mut fake = FakeRunner::default();
        let result = publish_release(
            &mut fake, &root, "v1.0.0", "sha", &notes, "notes", &assets, false,
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(fake.publish_called);
        let create = fake
            .calls
            .iter()
            .find(|(p, a)| p == "gh" && a.get(1).map(String::as_str) == Some("create"))
            .map(|(_, a)| a)
            .expect("create call");
        assert_eq!(
            create.get(0..7).unwrap(),
            [
                "release",
                "create",
                "v1.0.0",
                "--verify-tag",
                "--target",
                "sha",
                "--draft"
            ]
        );
        assert_eq!(create.get(7), Some(&"--notes-file".to_string()));
        assert_eq!(create.get(8), Some(&notes.to_string_lossy().into_owned()));
        assert_eq!(
            &create[9..],
            &assets
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .chain(std::iter::once(
                    assets[0]
                        .parent()
                        .unwrap()
                        .join("SHA256SUMS")
                        .to_string_lossy()
                        .into_owned()
                ))
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn publish_postcreate_metadata_mismatch_fails_before_edit() {
        let root = tempfile_dir();
        let mut fake = FakeRunner {
            postcreate_release: Some(release_json(true, "other", "notes")),
            ..Default::default()
        };
        let result = run_publish(&mut fake, None, &root);
        assert!(result.is_err());
        assert!(!fake.publish_called);
        assert!(
            !fake
                .calls
                .iter()
                .any(|(_, a)| a.get(1).map(String::as_str) == Some("download"))
        );
    }
    #[test]
    fn publish_download_corruption_fails_before_edit() {
        let root = tempfile_dir();
        let mut fake = FakeRunner {
            release: Some(release_json(true, "sha", "notes")),
            corrupt_download: true,
            ..Default::default()
        };
        let release = fake.release.clone();
        let result = run_publish(&mut fake, release, &root);
        assert!(result.is_err());
        assert!(!fake.publish_called);
    }
    #[test]
    fn publish_remote_peel_and_adversarial_args_are_recorded() {
        let mut fake = FakeRunner::default();
        let root = tempfile_dir();
        let notes = root.join("notes;--bad");
        fs::write(&notes, "notes").unwrap();
        let err = publish_release(
            &mut fake,
            &root,
            "v1.0.0;bad",
            "sha",
            &notes,
            "notes",
            &[],
            false,
        )
        .unwrap_err();
        assert!(err.contains("HEAD") || err.contains("remote") || err.contains("release"));
        assert!(
            fake.calls
                .iter()
                .all(|(_, args)| args.iter().all(|a| !a.contains("; ")))
        );
    }
    #[test]
    fn publish_race_create_failure_is_closed() {
        let root = tempfile_dir();
        let notes = root.join("notes");
        fs::write(&notes, "notes").unwrap();
        let assets = local_assets(&root);
        let mut fake = FakeRunner {
            fail_create: true,
            ..Default::default()
        };
        assert!(
            publish_release(
                &mut fake, &root, "v1.0.0", "sha", &notes, "notes", &assets, false
            )
            .is_err()
        );
        assert!(!fake.publish_called);
    }

    #[test]
    fn four_platform_asset_inventory() {
        assert_eq!(
            archive_names(),
            [
                "sqlite-mcp-x86_64-unknown-linux-gnu.tar.gz",
                "sqlite-mcp-aarch64-unknown-linux-gnu.tar.gz",
                "sqlite-mcp-aarch64-apple-darwin.tar.gz",
                "sqlite-mcp-x86_64-apple-darwin.tar.gz",
            ]
        );
    }

    #[test]
    fn asset_set_rejects_each_missing_archive() {
        let root = tempfile_dir();
        for missing in archive_names() {
            let dir = root.join(missing.replace('.', "-"));
            fs::create_dir_all(&dir).unwrap();
            for name in archive_names().iter().filter(|name| **name != missing) {
                fs::write(dir.join(name), b"x").unwrap();
            }
            let sums = dir.join("SHA256SUMS");
            fs::write(&sums, "").unwrap();
            assert!(
                verify_asset_dir(&dir).is_err(),
                "accepted missing {missing}"
            );
        }
    }

    #[test]
    fn asset_set_rejects_extra_archive() {
        let root = tempfile_dir();
        let dir = root.join("extra");
        fs::create_dir_all(&dir).unwrap();
        for name in archive_names() {
            fs::write(dir.join(name), b"x").unwrap();
        }
        fs::write(dir.join("extra.tar.gz"), b"x").unwrap();
        fs::write(dir.join("SHA256SUMS"), b"").unwrap();
        assert!(verify_asset_dir(&dir).is_err());
    }

    #[test]
    fn checksum_duplicate_and_omit_rejected() {
        let names = archive_names();
        let duplicate = format!("{:064x}  {}\n{:064x}  {}\n", 1, names[0], 2, names[0]);
        assert!(verify_checksums(&duplicate, &names).is_err());
        let omitted = names[..3]
            .iter()
            .map(|n| format!("{:064x}  {n}\n", 1))
            .collect::<String>();
        assert!(verify_checksums(&omitted, &names).is_err());
    }

    #[derive(Default)]
    struct HttpFake {
        calls: Vec<Vec<String>>,
        statuses: Vec<String>,
        bodies: Vec<Vec<u8>>,
        sleeps: Vec<Duration>,
        error: Option<String>,
    }
    impl CommandRunner for HttpFake {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
            _: Option<&Path>,
        ) -> Result<CommandOutput, String> {
            assert_eq!(program, "curl");
            self.calls.push(args.to_vec());
            if let Some(error) = self.error.take() {
                if let Some(path) = args
                    .windows(2)
                    .find(|pair| pair[0] == "--output")
                    .map(|pair| PathBuf::from(&pair[1]))
                {
                    fs::write(path, b"partial").unwrap();
                }
                return Err(error);
            }
            let status = self
                .statuses
                .first()
                .cloned()
                .unwrap_or_else(|| "200".into());
            if !self.statuses.is_empty() {
                self.statuses.remove(0);
            }
            if let Some(path) = args
                .windows(2)
                .find(|pair| pair[0] == "--output")
                .map(|pair| PathBuf::from(&pair[1]))
            {
                let body = if self.bodies.is_empty() {
                    Vec::new()
                } else {
                    self.bodies.remove(0)
                };
                fs::write(path, body).unwrap();
            }
            Ok(output(0, &status, ""))
        }
        fn sleep(&mut self, duration: Duration) {
            self.sleeps.push(duration);
        }
    }

    #[test]
    fn public_http_request_contract() {
        let mut fake = HttpFake {
            statuses: vec!["200".into()],
            ..Default::default()
        };
        let path = tempfile_dir().join("body");
        fs::write(&path, b"old").unwrap();
        anonymous_http_get(&mut fake, "https://example.invalid/a", &path).unwrap();
        let args = &fake.calls[0];
        assert!(args.windows(2).any(|x| x == ["--proto", "=https"]));
        assert!(args.windows(2).any(|x| x == ["--proto-redir", "=https"]));
        assert!(args.windows(2).any(|x| x == ["--connect-timeout", "10"]));
        assert!(args.windows(2).any(|x| x == ["--max-time", "30"]));
        assert!(
            !args
                .iter()
                .any(|x| x.contains("TOKEN") || x.contains("Authorization"))
        );
    }

    #[test]
    fn public_http_retry_policy() {
        let mut fake = HttpFake {
            statuses: vec![
                "404".into(),
                "502".into(),
                "503".into(),
                "504".into(),
                "200".into(),
            ],
            ..Default::default()
        };
        let path = tempfile_dir().join("body");
        anonymous_http_get(&mut fake, "https://example.invalid/a", &path).unwrap();
        assert_eq!(fake.calls.len(), 5);
        assert_eq!(fake.sleeps, vec![Duration::from_secs(2); 4]);
        let mut fail = HttpFake {
            statuses: vec!["400".into()],
            ..Default::default()
        };
        assert!(anonymous_http_get(&mut fail, "https://example.invalid/a", &path).is_err());
        assert!(fail.sleeps.is_empty());
    }

    #[test]
    fn public_http_runner_error_removes_partial_output() {
        let root = tempfile_dir();
        let path = root.join("partial");
        let mut fake = HttpFake {
            error: Some("runner failed".into()),
            ..Default::default()
        };
        assert!(anonymous_http_get(&mut fake, "https://example.invalid/a", &path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn public_http_rejects_downgrade_url() {
        let mut fake = HttpFake::default();
        let path = tempfile_dir().join("body");
        assert!(anonymous_http_get(&mut fake, "http://example.invalid/a", &path).is_err());
    }

    fn public_fixture(root: &Path, tag: &str, sha: &str) -> Vec<String> {
        let notes = release_notes(root, tag).unwrap();
        let assets = release_names()
            .iter()
            .map(|name| serde_json::json!({
                "name": name,
                "browser_download_url": format!("https://github.com/{PUBLIC_REPO}/releases/download/{tag}/{name}"),
                "size": 1
            }))
            .collect::<Vec<_>>();
        vec![
            serde_json::json!({"tag_name": tag, "draft": false, "prerelease": false, "published_at": "2026-01-02T00:00:00Z", "body": notes, "assets": assets}).to_string(),
            serde_json::json!({"object": {"sha": "tag-object", "type": "tag"}}).to_string(),
            serde_json::json!({"tag": tag, "object": {"sha": sha, "type": "commit"}}).to_string(),
        ]
    }

    #[test]
    fn public_release_metadata_success_records_evidence() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let evidence = tempfile_dir().join("evidence.json");
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let mut fake = FakeRunner {
            public_responses: public_fixture(&root, "v0.1.0", sha),
            ..Default::default()
        };
        verify_public_release(&mut fake, &root, "v0.1.0", sha, &evidence).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence).unwrap()).unwrap();
        assert_eq!(value["tag"], "v0.1.0");
        assert_eq!(value["expected_sha"], sha);
        assert_eq!(
            value["release_url"],
            "https://github.com/tilmon-engineering/sqlite-mcp/releases/tag/v0.1.0"
        );
        let urls = value["asset_urls"].as_array().unwrap();
        assert_eq!(urls.len(), release_names().len());
        for (url, name) in urls.iter().zip(release_names()) {
            assert_eq!(
                url,
                &format!("https://github.com/{PUBLIC_REPO}/releases/download/v0.1.0/{name}")
            );
        }
        let assertions = value["assertions"].as_array().unwrap();
        assert!(assertions.iter().any(|v| v == "release metadata"));
        assert!(assertions.iter().any(|v| v == "annotated tag peel"));
        assert!(assertions.iter().any(|v| v == "exact assets"));
        assert!(assertions.iter().any(|v| v == "workspace version 0.1.0"));
    }

    #[test]
    fn public_release_metadata_accepts_trailing_newline_body() {
        // GitHub canonicalizes stored release bodies with one trailing
        // newline; notes equality ignores only that canonical line ending.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let evidence = tempfile_dir().join("evidence.json");
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let mut responses = public_fixture(&root, "v0.1.0", sha);
        let mut release: serde_json::Value = serde_json::from_str(&responses[0]).unwrap();
        let body = release["body"].as_str().unwrap().to_owned();
        release["body"] = serde_json::json!(format!("{body}\n"));
        responses[0] = release.to_string();
        let mut fake = FakeRunner {
            public_responses: responses,
            ..Default::default()
        };
        verify_public_release(&mut fake, &root, "v0.1.0", sha, &evidence).unwrap();
        assert!(evidence.exists());
    }

    #[test]
    fn public_release_metadata_rejects_mismatch() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let cases = [
            "wrong_sha",
            "wrong_tag_name",
            "missing_tag_name",
            "wrong_tag_object_type",
            "wrong_tag_object_sha",
            "missing_release_body",
            "missing_annotated_object",
            "lightweight",
            "tag_name",
            "body",
            "draft",
            "prerelease",
            "unpublished",
            "missing_asset",
            "extra_asset",
            "empty_asset",
            "wrong_url",
            "malformed",
        ];
        for case in cases {
            let mut responses = public_fixture(&root, "v0.1.0", sha);
            let mut release: serde_json::Value = serde_json::from_str(&responses[0]).unwrap();
            match case {
                "wrong_sha" => responses[2] = serde_json::json!({"tag":"v0.1.0","object":{"sha":"ffffffffffffffffffffffffffffffffffffffff","type":"commit"}}).to_string(),
                "wrong_tag_name" => responses[2] = serde_json::json!({"tag":"v9.9.9","object":{"sha":sha,"type":"commit"}}).to_string(),
                "missing_tag_name" => responses[2] = serde_json::json!({"object":{"sha":sha,"type":"commit"}}).to_string(),
                "wrong_tag_object_type" => responses[2] = serde_json::json!({"tag":"v1.0.0","object":{"sha":sha,"type":"tree"}}).to_string(),
                "wrong_tag_object_sha" => responses[1] = serde_json::json!({"object":{"type":"tag"}}).to_string(),
                "missing_release_body" => { release.as_object_mut().unwrap().remove("body"); },
                "missing_annotated_object" => responses[2] = serde_json::json!({"tag":"v0.1.0"}).to_string(),
                "lightweight" => responses[1] = serde_json::json!({"object":{"sha":sha,"type":"commit"}}).to_string(),
                "tag_name" => release["tag_name"] = serde_json::json!("v9.9.9"),
                "body" => release["body"] = serde_json::json!("wrong"),
                "draft" => release["draft"] = serde_json::json!(true),
                "prerelease" => release["prerelease"] = serde_json::json!(true),
                "unpublished" => release["published_at"] = serde_json::Value::Null,
                "missing_asset" => { release["assets"].as_array_mut().unwrap().pop(); },
                "extra_asset" => release["assets"].as_array_mut().unwrap().push(serde_json::json!({"name":"extra","browser_download_url":"x","size":1})),
                "empty_asset" => release["assets"][0]["size"] = serde_json::json!(0),
                "wrong_url" => release["assets"][0]["browser_download_url"] = serde_json::json!("https://wrong.invalid"),
                "malformed" => responses[0] = "{".into(),
                _ => unreachable!(),
            }
            if case != "malformed" {
                responses[0] = release.to_string();
            }
            let evidence = tempfile_dir().join("must-not-exist.json");
            let mut fake = FakeRunner {
                public_responses: responses,
                ..Default::default()
            };
            assert!(
                verify_public_release(&mut fake, &root, "v0.1.0", sha, &evidence).is_err(),
                "accepted {case}"
            );
            assert!(!evidence.exists(), "wrote evidence for {case}");
        }
    }

    #[test]
    fn public_release_metadata_http_failure() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let evidence = tempfile_dir().join("must-not-exist.json");
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let mut fake = FakeRunner {
            public_http_failure: true,
            ..Default::default()
        };
        assert!(verify_public_release(&mut fake, &root, "v0.1.0", sha, &evidence).is_err());
        assert!(!evidence.exists());
    }

    #[test]
    fn fetch_public_assets_exact_inventory() {
        let root = tempfile_dir();
        let dir = root.join("assets");
        let payloads: Vec<Vec<u8>> = archive_names()
            .iter()
            .enumerate()
            .map(|(i, _)| format!("archive-{i}").into_bytes())
            .collect();
        let mut bodies = payloads.clone();
        let sums = archive_names()
            .iter()
            .zip(&payloads)
            .map(|(name, body)| {
                let p = root.join(name);
                fs::write(&p, body).unwrap();
                format!("{}  {name}\n", sha256(&p).unwrap())
            })
            .collect::<String>();
        bodies.push(sums.into_bytes());
        let mut fake = HttpFake {
            bodies,
            ..Default::default()
        };
        let assets = fetch_public_assets(&mut fake, "v0.1.0", &dir).unwrap();
        assert_eq!(assets.len(), archive_names().len());
        assert_eq!(fs::read_dir(dir).unwrap().count(), release_names().len());
    }

    #[test]
    fn fetch_public_assets_failure_no_success() {
        let root = tempfile_dir();
        let dir = root.join("assets");
        let mut fake = HttpFake {
            statuses: vec!["500".into()],
            ..Default::default()
        };
        assert!(fetch_public_assets(&mut fake, "v0.1.0", &dir).is_err());
        assert!(!dir.exists());
    }

    #[test]
    fn fetch_public_assets_rejects_invalid_tag_without_http() {
        let root = tempfile_dir();
        let dir = root.join("assets");
        let mut fake = HttpFake::default();
        assert!(fetch_public_assets(&mut fake, "v1.0.0/../../evil", &dir).is_err());
        assert!(fake.calls.is_empty());
        assert!(!dir.exists());
    }
}
