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
//! feishu ×2, gitter, gotify, irc, linkedin, mqtt, mumble, ntfy).
//!
//! # What this test actually checks
//!
//! It reads every `.rs` file under `crates/openfang-channels/src`, recursively,
//! and inspects every `select!` arm whose future is `<ident>.changed()`. An arm
//! is judged against the `select!` it belongs to: only a `select!` that the
//! source re-enters — one lexically inside a `loop`/`while`/`for` in the same
//! task — can spin, so those arms must *end* on a closed channel, while a
//! one-shot `select!` (the axum webhook servers: dingtalk, line, feishu, teams,
//! wecom, flock, messenger, pumble, viber, webhook) may fall through.
//!
//! Accepted in a re-entered `select!`:
//!
//! * `_ = rx.changed() => { ...; break; }` — the result is discarded, so the
//!   arm may not ask what the value is; it has to leave the loop
//!   unconditionally (`break`/`return` at the top level of the arm body).
//! * `changed = rx.changed() => { if changed.is_err() || *rx.borrow() { ...
//!   break; } }` — `Err` short-circuits into the same exit as `true`.
//! * `changed = rx.changed() => changed.is_err(),` — the block-less form, whose
//!   value *is* the Err-awareness (telegram's `sleep_or_shutdown`).
//!
//! Rejected, each shape having been checked to actually spin:
//!
//! * discarding the result and then branching on `*rx.borrow()`;
//! * discarding the result and not leaving the loop at all, or leaving it only
//!   under an `if` (the `break;`-deleted variant of the bluesky arm);
//! * binding the result and never calling `is_err()` on it;
//! * binding it and defusing the check — `!changed.is_err() && ...`,
//!   `changed.is_err() && ...` — so that `Err` no longer reaches the exit;
//! * binding it, testing `is_err()`, and not exiting on it.
//!
//! # What it does not check
//!
//! An arm whose future is written some other way (`(&mut rx).changed()`,
//! a `changed()` hidden behind a helper) is not recognised as a shutdown arm
//! and is not audited. The `audited >= 40` / `in_loop >= 25` floors below catch
//! the parser silently going blind — they do not catch a hand-written evasion.
//! This is a lint over one known shape, not a proof of absence.

use std::path::{Path, PathBuf};

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, recursively.
///
/// Recursive on purpose: the crate is flat today, so a `src/adapters/foo.rs`
/// added tomorrow would otherwise be exempt from the audit without anyone
/// noticing.
fn rust_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{dir:?}: {e}"));
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|x| x == "rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&src_dir(), &mut out);
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

/// The arm's own body — the text between its braces, and nothing else.
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
                '{' => {
                    depth += 1;
                    if depth == 1 {
                        continue; // the arm's own opening brace
                    }
                }
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

/// Indentation of `line` in columns, or `None` for blank/comment lines.
fn code_indent(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("/*") {
        return None;
    }
    Some(line.len() - trimmed.len())
}

/// Is the `select!` that owns the arm at `header_idx` re-entered by a loop?
///
/// Only a re-entered `select!` can spin: the arm falls through, control reaches
/// the top of the loop, and the same permanently-ready `changed()` future is
/// polled again. A `select!` that runs once (an axum server racing its shutdown
/// signal) may fall through into the end of the task.
///
/// Walks outward by indentation from the `select!` header: each successive line
/// above it that is less indented is an enclosing block header. `loop`/`while`/
/// `for` says re-entered; a task or function boundary (`fn`, `async move {`,
/// `spawn(`) ends the search, because a loop *outside* that boundary does not
/// re-enter this `select!` — it spawns another task.
fn select_is_in_loop(lines: &[&str], header_idx: usize) -> bool {
    let mut sel_idx = None;
    for idx in (0..header_idx).rev() {
        if lines[idx].contains("select!") {
            sel_idx = Some(idx);
            break;
        }
    }
    let Some(sel_idx) = sel_idx else {
        return false;
    };
    let Some(mut inner) = code_indent(lines[sel_idx]) else {
        return false;
    };

    for idx in (0..sel_idx).rev() {
        let Some(indent) = code_indent(lines[idx]) else {
            continue;
        };
        if indent >= inner {
            continue;
        }
        inner = indent;
        let trimmed = lines[idx].trim();
        if trimmed.starts_with("loop")
            || trimmed.starts_with("while ")
            || trimmed.starts_with("for ")
        {
            return true;
        }
        if trimmed.contains("fn ")
            || trimmed.contains("async move")
            || trimmed.contains("async {")
            || trimmed.contains("spawn(")
            || trimmed.starts_with("impl ")
            || trimmed.starts_with("mod ")
        {
            return false;
        }
    }
    false
}

/// Strip string literals and line comments so keyword searches see code only.
///
/// Without this, `info!("...break out...")` counts as an exit and a commented
/// out `break;` counts as a live one.
fn code_only(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                // Skip to the closing quote, honouring backslash escapes.
                while let Some(c) = chars.next() {
                    if c == '\\' {
                        chars.next();
                    } else if c == '"' {
                        break;
                    }
                }
                out.push(' ');
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                out.push('\n');
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Does `word` occur in `text` as a whole token?
fn has_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Does the arm body leave the loop unconditionally?
///
/// "Unconditionally" is the point: a `break` nested inside an `if` is reachable
/// only when the `if` is true, and for a `_ =` arm there is nothing left to test
/// but the *value*, which a closed channel leaves at `false`. So the exit must
/// sit at the arm body's own brace depth.
fn exits_unconditionally(body: &str) -> bool {
    let code = code_only(body);
    let mut depth = 0i32;
    let mut top = String::new();
    for ch in code.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {
                if depth == 0 {
                    top.push(ch);
                }
            }
        }
    }
    has_word(&top, "break") || has_word(&top, "return")
}

#[derive(Debug)]
struct Offender {
    location: String,
    reason: String,
}

/// Audit one source file. Split out from the walk so the shapes this test
/// exists to reject can be fed in directly — see the tests below.
fn audit_source(file: &str, text: &str) -> Vec<Offender> {
    let mut offenders = Vec::new();
    let lines: Vec<&str> = text.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        let Some((binding, receiver)) = parse_arm(line) else {
            continue;
        };
        let location = format!("{file}:{}", idx + 1);
        let body = arm_body(&lines, idx);
        let code = code_only(&body);
        let in_loop = select_is_in_loop(&lines, idx);
        let mut fail = |reason: String| offenders.push(Offender { location: location.clone(), reason });

        if binding == "_" {
            // Result discarded: the arm cannot tell `Err` from a real change,
            // so it must not consult the value, and — if the `select!` is
            // re-entered — it must end the loop every time it fires.
            if code.contains(&format!("*{receiver}.borrow()")) {
                fail(
                    "discards changed() and then branches on borrow(): a closed \
                     channel falls through and spins"
                        .to_string(),
                );
            } else if in_loop && !exits_unconditionally(&body) {
                fail(
                    "discards changed() and does not leave the enclosing loop \
                     unconditionally: a closed channel re-arms a ready future"
                        .to_string(),
                );
            }
            continue;
        }

        // Named binding. The block-less form is the whole check by itself.
        let trimmed = code.trim().trim_end_matches(',').trim();
        if trimmed == format!("{binding}.is_err()") {
            continue;
        }

        let is_err = format!("{binding}.is_err()");
        let Some(pos) = code.find(&is_err) else {
            fail("binds the changed() result but never checks is_err()".to_string());
            continue;
        };
        if code.contains(&format!("!{is_err}")) {
            fail(format!(
                "negates {is_err}, so a closed channel takes the fall-through branch"
            ));
            continue;
        }
        // The Err test must reach the exit on its own. `&&` with anything means
        // it does not: on `Err` the other operand decides, and the only other
        // thing in scope is `borrow()`, which a closed channel leaves `false`.
        let guard_end = code[pos..].find('{').map(|i| pos + i).unwrap_or(code.len());
        let guard = &code[pos.saturating_sub(80)..guard_end];
        if guard.contains("&&") {
            fail(format!(
                "{is_err} is ANDed with another condition, so `Err` alone does not exit"
            ));
            continue;
        }
        if in_loop && !(has_word(&code[pos..], "break") || has_word(&code[pos..], "return")) {
            fail(format!(
                "{is_err} is tested but the branch does not leave the loop"
            ));
        }
    }

    offenders
}

#[test]
fn no_shutdown_arm_treats_a_closed_channel_as_keep_going() {
    let mut offenders: Vec<Offender> = Vec::new();
    let mut audited = 0usize;
    let mut in_loop = 0usize;

    for path in rust_sources() {
        let file = path
            .strip_prefix(src_dir())
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let text = std::fs::read_to_string(&path).expect("source file must be readable");
        let lines: Vec<&str> = text.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if parse_arm(line).is_some() {
                audited += 1;
                if select_is_in_loop(&lines, idx) {
                    in_loop += 1;
                }
            }
        }
        offenders.extend(audit_source(&file, &text));
    }

    // Floors, not counts: they fail loudly if the parser or the loop detector
    // goes blind, which would otherwise make this test pass vacuously.
    assert!(
        audited >= 40,
        "audit matched only {audited} shutdown arms — the parser has probably \
         stopped recognising them"
    );
    assert!(
        in_loop >= 25,
        "only {in_loop} of {audited} arms were classified as inside a loop — \
         the loop detector has probably stopped working, and every arm it \
         misses is exempted from the exit requirement"
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

    eprintln!("audited {audited} shutdown arms, {in_loop} of them inside a loop");
}

/// The audit must reject every shape it exists to reject.
///
/// Each case here is a one-line edit away from a real arm in this crate, and
/// each one passed the first version of this audit.
#[test]
fn audit_rejects_every_known_spin_shape() {
    // bluesky's arm with its `break;` deleted: logs, falls through, spins.
    let no_exit = r#"
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        info!("Bluesky adapter shutting down");
                    }
                    _ = tokio::time::sleep(poll_interval) => {}
                }

                if *shutdown_rx.borrow() {
                    break;
                }
            }
"#;
    assert_eq!(audit_source("no_exit.rs", no_exit).len(), 1, "{no_exit}");

    // mqtt's arm with the disjunction turned into a negated conjunction.
    let negated = r#"
                loop {
                    tokio::select! {
                        changed = shutdown_rx.changed() => {
                            if !changed.is_err() && *shutdown_rx.borrow() {
                                break;
                            }
                        }
                        msg = stream.next() => {}
                    }
                }
"#;
    assert_eq!(audit_source("negated.rs", negated).len(), 1, "{negated}");

    // The same, without the `!`: `Err` still cannot exit on its own.
    let anded = negated.replace("!changed.is_err() &&", "changed.is_err() &&");
    assert_eq!(audit_source("anded.rs", &anded).len(), 1, "{anded}");

    // The original FANG-40 shape.
    let borrows = r#"
                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                break;
                            }
                        }
                        msg = stream.next() => {}
                    }
                }
"#;
    assert_eq!(audit_source("borrows.rs", borrows).len(), 1, "{borrows}");

    // Binds the result, never asks whether it is an error.
    let unchecked = r#"
                loop {
                    tokio::select! {
                        changed = shutdown_rx.changed() => {
                            info!("changed: {changed:?}");
                            break;
                        }
                        msg = stream.next() => {}
                    }
                }
"#;
    assert_eq!(audit_source("unchecked.rs", unchecked).len(), 1, "{unchecked}");

    // Tests is_err(), then does not leave the loop.
    let no_break = r#"
                loop {
                    tokio::select! {
                        changed = shutdown_rx.changed() => {
                            if changed.is_err() || *shutdown_rx.borrow() {
                                warn!("shutdown observed");
                            }
                        }
                        msg = stream.next() => {}
                    }
                }
"#;
    assert_eq!(audit_source("no_break.rs", no_break).len(), 1, "{no_break}");

    // A `break` that only fires under an `if` is not an unconditional exit.
    let conditional_exit = r#"
                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            if now.elapsed() > grace {
                                break;
                            }
                        }
                        msg = stream.next() => {}
                    }
                }
"#;
    assert_eq!(
        audit_source("conditional_exit.rs", conditional_exit).len(),
        1,
        "{conditional_exit}"
    );
}

/// …and must accept every shape the crate legitimately uses, or it will be
/// worked around rather than obeyed.
#[test]
fn audit_accepts_the_correct_shapes() {
    let unconditional_break = r#"
                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            info!("Bluesky adapter shutting down");
                            break;
                        }
                        _ = tokio::time::sleep(poll_interval) => {}
                    }

                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
"#;
    assert!(audit_source("a.rs", unconditional_break).is_empty());

    let err_or_value = r#"
                loop {
                    tokio::select! {
                        changed = shutdown_rx.changed() => {
                            if changed.is_err() || *shutdown_rx.borrow() {
                                info!("adapter shutting down");
                                let _ = ws_tx.close().await;
                                return;
                            }
                        }
                        msg = stream.next() => {}
                    }
                }
"#;
    assert!(audit_source("b.rs", err_or_value).is_empty());

    // Block-less arm: the value of the arm is the Err-awareness.
    let block_less = r#"
    let senders_gone = tokio::select! {
        _ = tokio::time::sleep(d) => false,
        changed = shutdown.changed() => changed.is_err(),
    };
"#;
    assert!(audit_source("c.rs", block_less).is_empty());

    // One-shot `select!` (the webhook servers): nothing re-enters it, so
    // falling through is the end of the task, not a spin.
    let one_shot = r#"
        tokio::spawn(async move {
            tokio::select! {
                _ = server => {}
                _ = shutdown_rx.changed() => {
                    info!("DingTalk adapter shutting down");
                }
            }
            info!("DingTalk webhook server stopped");
        });
"#;
    assert!(audit_source("d.rs", one_shot).is_empty());
}

/// The parser itself, on the two arm headers that exist and the things that
/// look like them.
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

/// The helpers the verdicts rest on, tested where they can lie.
#[test]
fn exit_detection_ignores_strings_comments_and_nested_blocks() {
    assert!(exits_unconditionally("info!(\"stopping\"); break;"));
    assert!(exits_unconditionally("let _ = tx.close().await; return;"));
    // Inside an `if` — conditional, not an exit.
    assert!(!exits_unconditionally("if late { break; }"));
    // A `break` that only exists in a message or a comment is not an exit.
    assert!(!exits_unconditionally("warn!(\"break glass\");"));
    assert!(!exits_unconditionally("// break;"));
    // Not a keyword, just an identifier that starts with one.
    assert!(!exits_unconditionally("breaker.trip();"));
}

/// Loop detection decides which arms must exit, so it gets its own test.
#[test]
fn loop_detection_separates_re_entered_selects_from_one_shot_ones() {
    let looped: Vec<&str> = vec![
        "        loop {",
        "            tokio::select! {",
        "                _ = shutdown_rx.changed() => {",
        "                    break;",
        "                }",
        "            }",
        "        }",
    ];
    assert!(select_is_in_loop(&looped, 2));

    let one_shot: Vec<&str> = vec![
        "    tokio::spawn(async move {",
        "        tokio::select! {",
        "            _ = server => {}",
        "            _ = shutdown_rx.changed() => {}",
        "        }",
        "    });",
    ];
    assert!(!select_is_in_loop(&one_shot, 3));

    // A task spawned inside a loop: the loop starts new tasks, it does not
    // re-enter this `select!`.
    let spawned_in_loop: Vec<&str> = vec![
        "    loop {",
        "        tokio::spawn(async move {",
        "            tokio::select! {",
        "                _ = server => {}",
        "                _ = shutdown_rx.changed() => {}",
        "            }",
        "        });",
        "    }",
    ];
    assert!(!select_is_in_loop(&spawned_in_loop, 4));
}
