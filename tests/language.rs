//! The repository is written in English, and this keeps it that way.
//!
//! It exists because this project was built in conversation in Russian, and phrases from
//! that conversation kept landing in test fixtures and comments. Anyone reading the code
//! later has no way to act on a sentence they cannot read, and a message an agent might
//! print should not be in a language its user did not choose.
//!
//! The check is deliberately narrow: Cyrillic only. Prose here uses em dashes and curly
//! quotes, so banning non-ASCII outright would be wrong, and the encoding tests need
//! genuinely multi-byte samples to be worth anything. What is being guarded against is one
//! specific language leaking in, so that is what is looked for.
//!
//! A line that needs an exception says so with `lint-allow: cyrillic`.

use std::path::{Path, PathBuf};

/// Everything a reader or a user might end up seeing.
const ROOTS: &[&str] = &[
    "src",
    "tests",
    "examples",
    ".github",
    "README.md",
    "AGENTS.md",
    "Cargo.toml",
];

/// Opt-out marker, for a line that genuinely needs Cyrillic.
const ALLOW: &str = "lint-allow: cyrillic";

#[test]
fn the_repository_is_written_in_english() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut offences = Vec::new();

    for entry in ROOTS {
        collect(&root.join(entry), &root, &mut offences);
    }

    assert!(
        offences.is_empty(),
        "found Cyrillic in files that should be English:\n{}\n\n\
         Rewrite it in English, or mark the line with `{ALLOW}` if it genuinely belongs.",
        offences.join("\n")
    );
}

#[test]
fn the_check_can_actually_see_cyrillic() {
    // A linter that never fires is indistinguishable from one that is broken. These are
    // written as escapes so the file itself stays clean.
    assert!(has_cyrillic(
        "\u{043f}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}"
    ));
    assert!(has_cyrillic("mixed \u{0414} in"));
    assert!(!has_cyrillic("plain english"));

    // Everything the prose here actually uses stays acceptable.
    assert!(!has_cyrillic("em dash — curly ’quote’ … ellipsis"));
    assert!(!has_cyrillic("grüße 🌍 こんにちは"));
}

#[test]
fn the_opt_out_marker_is_honoured() {
    let allowed = format!("let sample = \"\u{0434}\u{0430}\"; // {ALLOW}");
    assert!(offending_lines("x.rs", &allowed).is_empty());

    let bare = "let sample = \"\u{0434}\u{0430}\";".to_string();
    assert_eq!(offending_lines("x.rs", &bare).len(), 1);
}

/// Walk a file or directory, gathering offending lines.
fn collect(path: &Path, root: &Path, offences: &mut Vec<String>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect(&entry.path(), root, offences);
        }
        return;
    }

    // Only text we author. Anything unreadable as UTF-8 is not prose.
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    let shown = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();

    // This file describes the rule, so it would otherwise report itself.
    if shown.ends_with("tests/language.rs") {
        return;
    }

    offences.extend(offending_lines(&shown, &contents));
}

fn offending_lines(name: &str, contents: &str) -> Vec<String> {
    contents
        .lines()
        .enumerate()
        .filter(|(_, line)| has_cyrillic(line) && !line.contains(ALLOW))
        .map(|(index, line)| format!("  {name}:{}: {}", index + 1, line.trim()))
        .collect()
}

/// Cyrillic, including the supplement block.
fn has_cyrillic(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(c, '\u{0400}'..='\u{04FF}' | '\u{0500}'..='\u{052F}'))
}
