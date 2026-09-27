//! The owner's name comes from `owner_name` in config (sm#1580): no server,
//! CLI or app source outside tests may spell it out.

use std::{
    fs,
    path::{Path, PathBuf},
};

const NAME: &str = "Rajesh";

fn sources(dir: &Path, extension: &str, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, extension, found);
        } else if path.extension().is_some_and(|ext| ext == extension)
            // Whole-file test modules (`foo/tests.rs`).
            && path.file_name().is_none_or(|name| name != "tests.rs")
        {
            found.push(path);
        }
    }
}

/// The source before its `#[cfg(test)] mod ...` block: test modules sit at
/// the end of a file in this crate.
fn non_test_part(text: &str) -> &str {
    let lines: Vec<&str> = text.lines().collect();
    let mut offset = 0;
    for (index, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]"
            && lines
                .get(index + 1)
                .is_some_and(|next| next.trim_start().starts_with("mod "))
        {
            return &text[..offset];
        }
        offset += line.len() + 1;
    }
    text
}

#[test]
fn no_literal_owner_name_outside_tests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), "rs", &mut files);
    let app = root.join("../../android-app/app/src/main");
    if app.exists() {
        sources(&app, "kt", &mut files);
    }
    let offenders: Vec<String> = files
        .iter()
        .flat_map(|path| {
            let text = fs::read_to_string(path).unwrap();
            let text = if path.extension().is_some_and(|ext| ext == "rs") {
                non_test_part(&text).to_owned()
            } else {
                text
            };
            text.lines()
                .enumerate()
                .filter(|(_, line)| line.contains(NAME))
                .map(|(number, line)| format!("{}:{}: {}", path.display(), number + 1, line.trim()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "hard-coded owner name:\n{}",
        offenders.join("\n")
    );
}
