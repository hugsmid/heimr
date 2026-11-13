//! The adapter guide quotes real code. Every code block in
//! `docs/writing-an-adapter.md` preceded by `<!-- quote: <path>#<anchor> -->`
//! must equal the region between `ANCHOR: <anchor>` and
//! `ANCHOR_END: <anchor>` in that file, ignoring indentation, code fences,
//! and doc-comment markers of regions inside doc comments. Code blocks
//! without a marker are not allowed (shell and plain-text blocks aside), so
//! no snippet can drift from the code it describes.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The guide lives in `docs/` at the workspace root.
fn guide_path() -> PathBuf {
    root().join("docs").join("writing-an-adapter.md")
}

fn strip_doc(line: &str) -> &str {
    let t = line.trim_start();
    for prefix in ["//! ", "/// ", "//!", "///"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return rest;
        }
    }
    line
}

fn dedent(lines: &[String]) -> Vec<String> {
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| l.get(indent..).unwrap_or("").trim_end().to_owned())
        .collect()
}

fn region(path: &str, anchor: &str) -> Vec<String> {
    let text = std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
    let start = format!("ANCHOR: {anchor}");
    let end = format!("ANCHOR_END: {anchor}");
    let mut inside = false;
    // Regions anchored inside doc comments (doc-test examples) lose their
    // comment markers; regions in code keep their doc comments.
    let mut in_doc = false;
    let mut out = vec![];
    for line in text.lines() {
        if line.contains(&end) {
            return dedent(&out);
        }
        if inside {
            let l = if in_doc { strip_doc(line) } else { line };
            if !l.trim_start().starts_with("```") {
                out.push(l.to_owned());
            }
        }
        if line.contains(&start) {
            inside = true;
            let t = line.trim_start();
            in_doc = t.starts_with("///") || t.starts_with("//!");
        }
    }
    panic!("{path}: anchor {anchor} not found or not closed");
}

#[test]
fn every_snippet_in_the_guide_is_real_code() {
    // Never skipped: the build's source set (including the Nix one) must
    // contain the guide.
    let path = guide_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("adapter guide {}: {e}", path.display()));
    let mut quote: Option<(String, String)> = None;
    let mut block: Option<(String, Vec<String>)> = None;
    let mut checked = 0;
    for (n, line) in text.lines().enumerate() {
        if let Some((lang, lines)) = &mut block {
            if line.trim_start().starts_with("```") {
                let lang = std::mem::take(lang);
                let got = dedent(lines);
                block = None;
                // Shell commands and plain text are not code to quote.
                if matches!(lang.as_str(), "" | "text" | "bash" | "sh") {
                    continue;
                }
                let Some((file, anchor)) = quote.take() else {
                    panic!(
                        "{}:{}: {lang} block without a quote marker",
                        path.display(),
                        n + 1
                    );
                };
                let want = region(&file, &anchor);
                assert_eq!(
                    got,
                    want,
                    "{}:{}: block differs from {file}#{anchor}",
                    path.display(),
                    n + 1
                );
                checked += 1;
            } else {
                lines.push(line.to_owned());
            }
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix("<!-- quote: ") {
            let target = rest.trim_end_matches("-->").trim();
            let (file, anchor) = target.split_once('#').unwrap();
            quote = Some((file.to_owned(), anchor.to_owned()));
        } else if let Some(lang) = line.trim_start().strip_prefix("```") {
            block = Some((lang.trim().to_owned(), vec![]));
        }
    }
    assert!(checked >= 8, "only {checked} quoted blocks found");
}
