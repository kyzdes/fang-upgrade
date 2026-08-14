//! Source audit: no `select!` arm may treat a closed shutdown channel as
//! "keep going".
//!
//! FANG-40 fixed a 100% CPU spin whose shape is entirely local to one arm:
//!
//! ```ignore
//! tokio::select! {
//!     _ = shutdown_rx.changed() => {
//!         if *shutdown_rx.borrow() { break; }   // <-- the bug
//!     }
//!     ...
//! }
//! ```
//!
//! `watch::Receiver::changed()` resolves with `Err` once every `Sender` is
//! dropped, and it keeps resolving with `Err` forever after. `borrow()` then
//! still returns the last *value* — `false` — so the arm falls through, the
//! enclosing `loop` re-enters `select!`, and the same already-ready future is
//! polled again. Measured on `ours` with a real adapter: 1.06 cores, for the
//! life of the process. Where the other arm of the `select!` is the adapter's
//! poll rather than a sleep (discourse), the same fall-through also removes the
//! only delay between HTTP requests.
//!
//! Counting by hand is how the first attempt at FANG-40 came to fix six arms
//! and describe that as the set, when the crate carried seventeen of them
//! (bridge, discord ×2, slack ×2, mattermost — then dingtalk_stream, discourse,
//! feishu ×2, gitter, gotify, irc, linkedin, mqtt, mumble, ntfy). This test is
//! the count: it fails on any arm still written the old way, so the invariant is
//! checked rather than asserted in a commit message, and a new adapter cannot
//! reintroduce the shape quietly.
//!
//! Two arm forms are accepted:
//!
//! * `_ = rx.changed() => { ...exit unconditionally... }` — never consults
//!   `borrow()`, so a closed channel ends the task like any other wakeup.
//! * `changed = rx.changed() => { if changed.is_err() || *rx.borrow() {...} }`
//!   — inspects the result and treats `Err` as shutdown.
//!
//! Rejected: an arm that discards the result (`_ =`) and then branches on
//! `*rx.borrow()`, or one that binds the result and never asks whether it is an
//! error.

use std::path::{Path, PathBuf};

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_sources() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(src_dir())
        .expect("crates/openfang-channels/src must be readable")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no .rs files found under {:?}", src_dir());
    out
}

/// Split `<binding> = <receiver>.changed() => {` into its two names.
///
/// Returns `None` for anything that is not a `select!` arm header on a
/// `changed()` future — `let _ = rx.changed().await;` in particular, which is
/// a plain await with no fall-through to spin.
fn parse_arm(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.starts_with("let ") || !trimmed.contains("=>") {
        return None;
    }
    let (lhs, _) = trimmed.split_once("=>")?;
    let (binding, call) = lhs.split_once('=')?;
    let call = call.trim();
    let receiver = call.strip_suffix(".changed()")?;
    if receiver.is_empty() || !receiver.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((binding.trim().to_string(), receiver.to_string()))
}

/// The arm's own body, and nothing else.
///
/// Brace-matched rather than "the next N lines", because several adapters
/// follow the `select!` with a second `if *shutdown_rx.borrow() { break; }`
/// belonging to the *sleep* arm's fall-through. A windowed reader attributes
/// that line to the shutdown arm above it and reports seven adapters that are
/// in fact correct. Arms with no block (`=> changed.is_err(),`) return the
/// trailing expression.
fn arm_body(lines: &[&str], header_idx: usize) -> String {
    let header = lines[header_idx];
    let arrow = header.find("=>").expect("caller already matched on `=>`");
    let after = &header[arrow + 2..];
    let Some(brace) = after.find('{') else {
        return after.trim().to_string();
    };

    let mut depth = 0i32;
    let mut out = String::new();
    let mut chunk = &after[brace..];
    let mut idx = header_idx;
    loop {
        for ch in chunk.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return out;
                    }
                }
                _ => {}
            }
            out.push(ch);
        }
        out.push('\n');
        idx += 1;
        if idx >= lines.len() {
            return out; // unbalanced source; the assertions below will judge it
        }
        chunk = lines[idx];
    }
}

struct Offender {
    location: String,
    reason: &'static str,
}

#[test]
fn no_shutdown_arm_treats_a_closed_channel_as_keep_going() {
    let mut offenders: Vec<Offender> = Vec::new();
    let mut audited = 0usize;
    let mut result_inspecting = 0usize;

    for path in rust_sources() {
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("source file must be readable");
        let lines: Vec<&str> = text.lines().collect();

        for (idx, line) in lines.iter().enumerate() {
            let Some((binding, receiver)) = parse_arm(line) else {
                continue;
            };
            audited += 1;
            let location = format!("{file}:{}", idx + 1);

            let body = arm_body(&lines, idx);
            let consults_value = body.contains(&format!("*{receiver}.borrow()"));

            if binding == "_" {
                // Result discarded. Fine only if the arm never asks what the
                // value is — then `Err` and `true` take the same path out.
                if consults_value {
                    offenders.push(Offender {
                        location,
                        reason: "discards changed() and then branches on borrow(): \
                                 a closed channel falls through and spins",
                    });
                }
            } else {
                result_inspecting += 1;
                if !body.contains(&format!("{binding}.is_err()")) {
                    offenders.push(Offender {
                        location,
                        reason: "binds the changed() result but never checks is_err()",
                    });
                }
            }
        }
    }

    assert!(
        audited >= 40,
        "audit matched only {audited} shutdown arms — the parser has probably \
         stopped recognising them, which would make this test pass vacuously"
    );

    assert!(
        offenders.is_empty(),
        "{} shutdown arm(s) can spin on a closed watch channel (FANG-40):\n{}",
        offenders.len(),
        offenders
            .iter()
            .map(|o| format!("  {} — {}", o.location, o.reason))
            .collect::<Vec<_>>()
            .join("\n")
    );

    eprintln!("audited {audited} shutdown arms, {result_inspecting} of them Err-aware");
}

/// The audit must actually reject the shape it exists to reject.
///
/// Without this, a parser change that silently stopped matching anything would
/// leave `no_shutdown_arm_treats_a_closed_channel_as_keep_going` green forever.
#[test]
fn parse_arm_recognises_both_forms_and_ignores_plain_awaits() {
    assert_eq!(
        parse_arm("                    _ = shutdown_rx.changed() => {"),
        Some(("_".to_string(), "shutdown_rx".to_string()))
    );
    assert_eq!(
        parse_arm("        changed = hb_shutdown.changed() => {"),
        Some(("changed".to_string(), "hb_shutdown".to_string()))
    );
    // Plain await, not a select arm — no fall-through, nothing to spin.
    assert_eq!(parse_arm("            let _ = shutdown_rx.changed().await;"), None);
    // A different future in the same select! must not be mistaken for one.
    assert_eq!(parse_arm("                    _ = tokio::time::sleep(d) => {}"), None);
}

/// `arm_body` must stop at the arm's closing brace.
///
/// The seven poll-loop adapters (bluesky, keybase, mastodon, nextcloud, reddit,
/// rocketchat, twist) put `if *shutdown_rx.borrow() { break; }` immediately
/// *after* the `select!`, as the sleep arm's fall-through. Reading a fixed
/// window of lines instead of the braces reports all seven as broken.
#[test]
fn arm_body_stops_at_the_arms_closing_brace() {
    let lines = vec![
        "        _ = shutdown_rx.changed() => {",
        "            info!(\"shutting down\");",
        "            break;",
        "        }",
        "        _ = tokio::time::sleep(poll_interval) => {}",
        "    }",
        "",
        "    if *shutdown_rx.borrow() {",
        "        break;",
        "    }",
    ];
    let body = arm_body(&lines, 0);
    assert!(body.contains("break;"), "body: {body}");
    assert!(
        !body.contains("*shutdown_rx.borrow()"),
        "body leaked past the arm: {body}"
    );

    // Block-less arm: the trailing expression is the body.
    let one_liner = vec!["        changed = shutdown.changed() => changed.is_err(),"];
    assert_eq!(arm_body(&one_liner, 0), "changed.is_err(),");
}
