//! Check the paths an agent's turn names against the filesystem.
//!
//! FANG-9: a turn can end with "I wrote the summary to output/report.md" and
//! no file at that path, and the runtime forwards the sentence to the caller
//! as its own successful answer.
//!
//! This module does not read the sentence for meaning. It collects the
//! path-shaped tokens the turn produced — in the reply text, and in the
//! arguments of the tool calls the model asked for — and asks the filesystem
//! about each one. The only thing it ever reports is a definite "there is
//! nothing at that path". A path that exists, a path that leaves the
//! workspace, an absent workspace root and an I/O error all produce nothing,
//! because "cannot check" is not "does not exist".
//!
//! Every rule below is exercised by the tests at the bottom of this file.

use std::path::{Component, Path, PathBuf};

/// Tool-call argument keys whose string value is taken as a filesystem path.
/// Matched case-insensitively against the top-level keys of the tool input.
const PATH_ARG_KEYS: &[&str] = &[
    "path",
    "file_path",
    "filepath",
    "filename",
    "output_path",
    "target_path",
    "dest",
    "destination",
];

/// Dotted suffixes that end a hostname rather than a filename. Without this
/// list `example.com` in "I fetched it from example.com" is a path-shaped
/// token like any other, and the check would answer about a file nobody meant.
const HOSTNAME_SUFFIXES: &[&str] = &[
    "com", "org", "net", "edu", "gov", "mil", "int", "info", "biz", "io", "co", "ai", "dev", "app",
    "xyz", "ru", "uk", "de", "fr", "cn", "jp", "eu", "us",
];

/// Punctuation that ends a token inside a whitespace-delimited chunk. It is
/// what wraps a path in prose and in Markdown, so `[report](output/report.md)`
/// yields `output/report.md` and not `report](output/report.md`.
fn is_token_boundary(c: char) -> bool {
    "()[]{}<>\"'`,;:!?*|".contains(c)
}

/// At most this many candidates are checked, and at most this many missing
/// paths are named in the note.
const MAX_CANDIDATES: usize = 16;
const MAX_NAMED: usize = 5;

/// True when a token is shaped like a path to a file inside a workspace.
///
/// Two shapes qualify: anything containing `/`, and a bare `stem.ext` whose
/// extension is 2–8 characters, carries at least one letter, and is not a
/// hostname suffix. The length and letter rules are what keep `1.2` (from
/// "Wrote 1.2 KB to …"), `v1.0` and `e.g.` out.
fn looks_like_path(token: &str) -> bool {
    if token.is_empty() || token.contains("://") || token.contains('@') {
        return false;
    }
    let first_segment = token.split('/').next().unwrap_or("");
    if let Some((stem, ext)) = first_segment.rsplit_once('.') {
        if !stem.is_empty() && HOSTNAME_SUFFIXES.contains(&ext.to_ascii_lowercase().as_str()) {
            return false;
        }
    }
    if token.contains('/') {
        return token.chars().any(|c| c.is_ascii_alphanumeric());
    }
    match token.rsplit_once('.') {
        Some((stem, ext)) => {
            stem.chars().any(|c| c.is_ascii_alphanumeric())
                && (2..=8).contains(&ext.chars().count())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

/// Every path-shaped token in a piece of prose, in the order it appears,
/// without duplicates.
pub fn paths_named_in_text(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for chunk in text.split_whitespace() {
        // A URL or an address is discarded whole. Splitting it on punctuation
        // first would leave `//github.com/x/y.git`, which is path-shaped.
        if chunk.contains("://") || chunk.contains('@') {
            continue;
        }
        for raw in chunk.split(is_token_boundary) {
            // Only trailing dots go: a leading dot is part of `.env` and `./out`.
            let token = raw.trim_end_matches('.');
            if !looks_like_path(token) {
                continue;
            }
            if !out.iter().any(|p| p == token) {
                out.push(token.to_string());
            }
        }
    }
    out
}

/// The path-valued arguments of one tool call, whether or not it ran.
///
/// The model asking for `file_write {"path": "output/report.md"}` names that
/// path just as plainly as writing it in a sentence, and it names it even
/// when the reply is "Saved!" with no path in the prose at all.
pub fn paths_in_tool_arguments(input: &serde_json::Value) -> Vec<String> {
    let Some(obj) = input.as_object() else {
        return Vec::new();
    };
    // Iterating the key list rather than the object keeps the order the same
    // whatever order the JSON map hands its entries back in.
    let mut out = Vec::new();
    for wanted in PATH_ARG_KEYS {
        for (key, value) in obj {
            if !key.eq_ignore_ascii_case(wanted) {
                continue;
            }
            if let Some(s) = value.as_str() {
                let s = s.trim();
                if !s.is_empty() && !out.iter().any(|p| p == s) {
                    out.push(s.to_string());
                }
            }
        }
    }
    out
}

/// Where a named path would live inside the workspace, or `None` when the
/// question cannot be asked about this workspace: a `..` component, or an
/// absolute path that does not start at the workspace root.
fn resolve_inside_workspace(raw: &str, workspace_root: &Path) -> Option<PathBuf> {
    let path = Path::new(raw);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    if path.is_absolute() {
        let canonical_root = workspace_root.canonicalize();
        let inside = path.starts_with(workspace_root)
            || canonical_root.as_ref().is_ok_and(|r| path.starts_with(r));
        return inside.then(|| path.to_path_buf());
    }
    Some(workspace_root.join(path))
}

/// The subset of `candidates` the filesystem answers "no" about.
///
/// `Ok(false)` from `try_exists` is the only answer that puts a path in the
/// result. `Ok(true)` means it is there; `Err` means the runtime could not
/// find out. Neither is reported, and neither is anything at all when
/// `workspace_root` is `None`.
pub async fn missing_paths(candidates: &[String], workspace_root: Option<&Path>) -> Vec<String> {
    let Some(root) = workspace_root else {
        return Vec::new();
    };
    let mut missing = Vec::new();
    for raw in candidates.iter().take(MAX_CANDIDATES) {
        let Some(resolved) = resolve_inside_workspace(raw, root) else {
            continue;
        };
        if matches!(tokio::fs::try_exists(&resolved).await, Ok(false)) {
            missing.push(raw.clone());
        }
    }
    missing
}

/// The sentence appended to the reply, built out of the checked paths and
/// nothing else. `None` when nothing was found missing.
pub fn workspace_check_note(missing: &[String]) -> Option<String> {
    let named: Vec<&str> = missing.iter().take(MAX_NAMED).map(String::as_str).collect();
    match named.as_slice() {
        [] => None,
        [one] => Some(format!("\n\n[Workspace check: {one} does not exist.]")),
        many => Some(format!(
            "\n\n[Workspace check: these paths do not exist: {}.]",
            many.join(", ")
        )),
    }
}

/// The whole check for one finished turn: gather the paths the turn named,
/// ask the filesystem, and return the note if any of them are not there.
pub async fn note_for_turn(
    reply_text: &str,
    requested_tool_inputs: &[serde_json::Value],
    workspace_root: Option<&Path>,
) -> Option<String> {
    // No workspace root, nothing knowable, nothing said.
    let root = workspace_root?;
    let mut candidates = paths_named_in_text(reply_text);
    for input in requested_tool_inputs {
        for path in paths_in_tool_arguments(input) {
            if !candidates.contains(&path) {
                candidates.push(path);
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    workspace_check_note(&missing_paths(&candidates, Some(root)).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---------------------------------------------------------- extraction --

    #[test]
    fn test_path_is_taken_out_of_prose_and_markdown() {
        assert_eq!(
            paths_named_in_text(
                "Done. I wrote the full summary to output/fang9-report.md and verified it."
            ),
            vec!["output/fang9-report.md"]
        );
        assert_eq!(
            paths_named_in_text("See [the report](output/report.md) for details."),
            vec!["output/report.md"]
        );
        assert_eq!(
            paths_named_in_text("Saved to `notes.txt`, as asked."),
            vec!["notes.txt"]
        );
        assert_eq!(
            paths_named_in_text("Report generated: output/report.md"),
            vec!["output/report.md"]
        );
    }

    #[test]
    fn test_tokens_that_are_not_paths_are_left_alone() {
        for text in [
            "Wrote 1.2 KB in total.",
            "This is version v1.0 of the plan.",
            "Use the API, e.g. through the client.",
            "I fetched it from example.com and from https://github.com/x/y.git",
            "Mail me at someone@example.org.",
            "Saved!",
            "Notification sent.",
        ] {
            assert!(
                paths_named_in_text(text).is_empty(),
                "extracted {:?} from: {text}",
                paths_named_in_text(text)
            );
        }
    }

    #[test]
    fn test_a_path_is_named_once_however_often_it_is_repeated() {
        assert_eq!(
            paths_named_in_text("I wrote a.md, then rewrote a.md, and a.md is final."),
            vec!["a.md"]
        );
    }

    #[test]
    fn test_tool_arguments_name_a_path_the_prose_omits() {
        let input = serde_json::json!({"path": "output/report.md", "content": "hi"});
        assert_eq!(paths_in_tool_arguments(&input), vec!["output/report.md"]);
        let renamed = serde_json::json!({"file_path": "a.txt", "destination": "b.txt"});
        assert_eq!(paths_in_tool_arguments(&renamed), vec!["a.txt", "b.txt"]);
        assert!(paths_in_tool_arguments(&serde_json::json!({"query": "x"})).is_empty());
        assert!(paths_in_tool_arguments(&serde_json::json!("not an object")).is_empty());
    }

    // ------------------------------------------------------- verification --

    #[tokio::test]
    async fn test_a_file_that_is_there_is_never_reported() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("output")).unwrap();
        std::fs::write(dir.path().join("output/report.md"), "real").unwrap();

        let note = note_for_turn(
            "Done. I wrote the full summary to output/report.md.",
            &[],
            Some(dir.path()),
        )
        .await;
        assert_eq!(note, None, "an existing file must never be marked");
    }

    /// The case the previous, text-classifying approach got wrong: the write
    /// happened in an earlier turn, so this turn ran no tool at all. The file
    /// is on disk, so the check passes it — no special code for "last turn".
    #[tokio::test]
    async fn test_a_file_written_in_an_earlier_turn_is_not_reported() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("output")).unwrap();
        std::fs::write(dir.path().join("output/report.md"), "written last turn").unwrap();

        // No tool calls this turn, and the reply repeats the earlier claim.
        let note = note_for_turn(
            "As I said, the summary is already at output/report.md.",
            &[],
            Some(dir.path()),
        )
        .await;
        assert_eq!(note, None);
    }

    #[tokio::test]
    async fn test_a_file_that_is_absent_is_reported_by_name() {
        let dir = TempDir::new().unwrap();
        let note = note_for_turn(
            "Done. I wrote the full summary to output/fang9-report.md and verified it.",
            &[],
            Some(dir.path()),
        )
        .await
        .expect("an absent named path must produce a note");
        assert!(
            note.contains("output/fang9-report.md"),
            "the note must name the path that was checked: {note}"
        );
        assert!(note.contains("does not exist"), "{note}");
    }

    #[tokio::test]
    async fn test_a_pathless_claim_is_caught_through_the_tool_it_asked_for() {
        let dir = TempDir::new().unwrap();
        let note = note_for_turn(
            "Saved!",
            &[serde_json::json!({"path": "output/report.md", "content": "x"})],
            Some(dir.path()),
        )
        .await
        .expect("the path came from the tool call the model asked for");
        assert!(note.contains("output/report.md"), "{note}");
    }

    #[tokio::test]
    async fn test_silence_when_there_is_no_workspace_to_check_against() {
        let note = note_for_turn(
            "Done. I wrote the full summary to output/fang9-report.md.",
            &[serde_json::json!({"path": "output/fang9-report.md"})],
            None,
        )
        .await;
        assert_eq!(note, None, "without a workspace root nothing is knowable");
    }

    #[tokio::test]
    async fn test_silence_for_a_path_that_leaves_the_workspace() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let absolute_outside = outside.path().join("nope.md");
        let candidates = vec![
            "../escape.md".to_string(),
            absolute_outside.display().to_string(),
        ];
        assert!(
            missing_paths(&candidates, Some(dir.path()))
                .await
                .is_empty(),
            "a path outside the workspace is unverifiable, not absent"
        );
    }

    #[tokio::test]
    async fn test_an_absolute_path_inside_the_workspace_is_checked() {
        let dir = TempDir::new().unwrap();
        let inside = dir.path().join("gone.md");
        let missing = missing_paths(&[inside.display().to_string()], Some(dir.path())).await;
        assert_eq!(missing, vec![inside.display().to_string()]);
    }

    #[tokio::test]
    async fn test_a_directory_counts_as_present() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("output")).unwrap();
        assert!(missing_paths(&["output".to_string()], Some(dir.path()))
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_note_lists_every_absent_path_it_checked() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("here.md"), "real").unwrap();
        let note = note_for_turn(
            "I wrote here.md and gone-a.md and gone-b.md.",
            &[],
            Some(dir.path()),
        )
        .await
        .unwrap();
        assert!(
            note.contains("gone-a.md") && note.contains("gone-b.md"),
            "{note}"
        );
        assert!(
            !note.contains("here.md"),
            "the file that exists must not appear in the note: {note}"
        );
    }

    // ------------------------------------------------------- measurement ---

    /// The corpus that measured the previous approach, kept as a measurement.
    /// Each row is a phantom claim written the way models actually word them,
    /// paired with the paths this module extracts from it. A row extracting
    /// nothing is a claim with no path in it: there is nothing to `stat`, and
    /// silence is the designed answer.
    const CLAIM_CORPUS: &[(&str, &[&str])] = &[
        // -- delivery: no filesystem path, so nothing to check --------------
        ("Done — I sent the summary to the Telegram channel.", &[]),
        ("I've posted the update to Slack.", &[]),
        ("The report has been emailed to you.", &[]),
        ("Notification sent.", &[]),
        ("I messaged the team on Slack.", &[]),
        ("Your message is on its way to Telegram.", &[]),
        ("I forwarded it to the ops channel.", &[]),
        ("Delivered to Discord.", &[]),
        ("I've notified the team by email.", &[]),
        ("I dropped it in the #general channel.", &[]),
        // -- file write: the path is there whatever the verb ----------------
        (
            "Done. I wrote the full summary to output/fang9-report.md and verified the contents on disk.",
            &["output/fang9-report.md"],
        ),
        ("I've saved the summary to output/report.md.", &["output/report.md"]),
        ("The file has been created at output/report.md.", &["output/report.md"]),
        ("I updated README.md with the new section.", &["README.md"]),
        ("All set — the summary is now at output/report.md.", &["output/report.md"]),
        ("I put the results in notes.txt.", &["notes.txt"]),
        ("I added the section to README.md.", &["README.md"]),
        ("Report generated: output/report.md", &["output/report.md"]),
        ("I've written it out to disk.", &[]),
        ("The summary now lives in output/report.md.", &["output/report.md"]),
        ("I exported the table to data.csv.", &["data.csv"]),
        ("Wrote 1.2 KB to output/report.md.", &["output/report.md"]),
        ("I've appended the log to run.log.", &["run.log"]),
        ("Saved!", &[]),
        // -- memory write: no filesystem path -------------------------------
        ("I stored that in memory for next time.", &[]),
        ("I've remembered your preference.", &[]),
        ("Noted in my knowledge base.", &[]),
        ("I saved it to memory.", &[]),
        ("I recorded it in my long-term memory.", &[]),
    ];

    /// Answers that report no completed work: refusals, failures, intent,
    /// questions, and the user's own action. Extraction treats them exactly
    /// like any other row — what keeps them safe is that the path they name
    /// is looked up, and a note is printed only for a path that is not there.
    /// `test_an_honest_answer_about_a_real_file_is_never_marked` is that half.
    const HONEST_CORPUS: &[&str] = &[
        "I could not write output/report.md — the workspace is read-only.",
        "I did not send it to Telegram; no channel is configured.",
        "Writing failed: permission denied on output/report.md.",
        "Nothing was stored in memory.",
        "I'll write the summary to output/report.md next.",
        "Should I save this to output/report.md?",
        "You saved it to notes.txt earlier.",
        "I failed to write the file because I lacked the necessary tool to \
         perform the write operation. No content was saved to \
         output/live-fabricated.md.",
        "I'm going to post this to Slack once you confirm.",
        "I tried to write output/report.md but the directory does not exist.",
    ];

    /// How much of the corpus carries something checkable. This is the
    /// measurement the design rests on: 12 of the 29 rows name a path, and
    /// all 12 are file-write claims — every file-write row that puts a path
    /// in its text is extracted, whatever verb it uses, including the four
    /// wordings the previous approach's verb lists missed ("is now at",
    /// "I put the results in", "I added the section to", "Report generated:").
    /// The 17 that extract nothing are the 10 delivery and 5 memory rows,
    /// which name no file, plus the two file rows that name none either
    /// ("I've written it out to disk.", "Saved!"). Delivery claims stay with
    /// the channel guard in `agent_loop::phantom_action_detected`; this
    /// module is about files.
    #[test]
    fn test_measured_extraction_on_natural_phrasings() {
        let mut with_a_path = 0usize;
        for (text, expected) in CLAIM_CORPUS {
            let got = paths_named_in_text(text);
            assert_eq!(
                got,
                expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "extraction changed for: {text}"
            );
            if !got.is_empty() {
                with_a_path += 1;
            }
        }
        assert_eq!(
            (with_a_path, CLAIM_CORPUS.len()),
            (12, 29),
            "the extraction rate quoted on test_measured_extraction_on_natural_phrasings \
             must match what the corpus measures"
        );
    }

    /// The rule that outranks every catch: an answer about a file that is
    /// really there is returned untouched. Every honest row that names a path
    /// is run against a workspace where that path exists.
    #[tokio::test]
    async fn test_an_honest_answer_about_a_real_file_is_never_marked() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("output")).unwrap();
        for text in HONEST_CORPUS {
            for path in paths_named_in_text(text) {
                let full = dir.path().join(&path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&full, "real").unwrap();
            }
            assert_eq!(
                note_for_turn(text, &[], Some(dir.path())).await,
                None,
                "an answer about a file that exists must be returned untouched: {text}"
            );
        }
    }
}
