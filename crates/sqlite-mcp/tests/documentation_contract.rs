//! Executable documentation contract for behavior fixes: each pinning
//! assertion fails when the required README phrase is removed, so the
//! documentation cannot silently drift from shipped behavior.

use std::fs;

fn readme() -> String {
    let repository_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::read_to_string(repository_root.join("README.md"))
        .unwrap_or_else(|error| panic!("read README.md: {error}"))
}

#[test]
fn readme_documents_sigint_exit() {
    let readme = readme();
    assert!(
        readme.contains(
            "Ctrl-C (SIGINT) triggers ordered worker shutdown, rolls back open transactions, and exits zero"
        ),
        "README must document SIGINT ordered shutdown and exit-zero behavior"
    );
}

#[test]
fn readme_documents_temp_schema_denial() {
    let readme = readme();
    assert!(
        readme
            .contains("Schema-qualified `temp.` objects (e.g. `CREATE TABLE temp.t`) are denied like TEMP objects"),
        "README must document temp-schema-qualified object denial"
    );
}

#[test]
fn readme_documents_maintenance_denial() {
    let readme = readme();
    assert!(
        readme.contains(
            "Maintenance operations (`ANALYZE`, `REINDEX`) and extension loading are denied"
        ),
        "README must document maintenance and extension-loading denial"
    );
}
