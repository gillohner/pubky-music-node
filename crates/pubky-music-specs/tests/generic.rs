//! The spec stays generic (STEWARD-SPEC-SPLIT.md): no app's own paths and no node crate. Every file of
//! the crate is read, so a new one is checked too.

fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "target") {
                files(&p, out);
            }
        } else {
            out.push(p);
        }
    }
}

#[test]
fn no_app_data_and_no_node_crates_in_the_spec() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = Vec::new();
    files(root, &mut all);
    assert!(all.len() > 3, "the crate's files weren't found");
    for f in &all {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        // This file names what it forbids
        if f.ends_with("generic.rs") {
            continue;
        }
        assert!(
            !text.contains("pubky.music/"),
            "{} names an app path (pubky.music/)",
            f.display()
        );
    }
    // No node crate in any form: `pm-x = …`, `[dependencies.pm-x]`, `package = "pm-x"`,
    // `path = "../pm-x"`. Comments don't count.
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    for line in manifest.lines() {
        let code = line.split('#').next().unwrap_or("");
        assert!(
            !code.contains("pm-") && !code.contains("pm_"),
            "Cargo.toml names a node crate: {line}"
        );
    }
}
