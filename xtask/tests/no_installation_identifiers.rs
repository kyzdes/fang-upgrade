//! Страж имени установки.
//!
//! Форк публичный: его исходники читают и поднимают у себя чужие люди. Строка,
//! называющая конкретную установку — её бренд, её домен, её WebAuthn-личность, —
//! попав в исходник, уезжает к каждому, кто соберёт этот код. Так и случилось:
//! страница входа несла `<div class="eyebrow">DenisAgency</div>`, а фикстуры
//! тестов — живой домен. Одной вычистки мало, потому что следующая правка
//! занесёт заново; поэтому запрет живёт тестом, а не памятью.
//!
//! Ловится два класса строк, и оба выбраны так, чтобы законное упоминание не
//! краснело:
//!
//! 1. **Имена оператора.** Точный список токенов: бренд заказчика и два домена
//!    его установок. Списком, а не эвристикой: слово «резерв» встречается в
//!    русской локали (`agents.none_fallback`) как обычное слово, и общий поиск
//!    по именам людей покраснел бы на нём.
//! 2. **WebAuthn relying party.** `rp_id`, `rp_origin` и `rp_name` — это и есть
//!    личность установки по определению: RP ID обязан совпадать с публичным
//!    доменом. Конкретное значение в исходнике всегда чей-то стенд, поэтому
//!    строковые литералы этих трёх полей обязаны быть либо пустыми, либо на
//!    зарезервированных именах (RFC 2606: `example.test`, `example.com`,
//!    `.invalid`, `localhost`), либо нейтральным названием продукта.
//!
//! Сам этот файл из обхода исключён: он обязан содержать запрещённые токены,
//! иначе ему нечем искать.

use std::path::{Path, PathBuf};

/// Имена конкретной установки. Сравнение регистронезависимое.
const FORBIDDEN_TOKENS: &[&str] = &["denisagency", "denis-openfang", "moone.dev"];

/// Значения `rp_*`, которые не называют ничьей установки.
const NEUTRAL_RP_NAMES: &[&str] = &["OpenFang", "OpenFang Example"];

/// Зарезервированные и локальные имена хостов (RFC 2606 / RFC 6761).
const RESERVED_HOST_MARKERS: &[&str] = &[
    "example.test",
    "example.com",
    "example.org",
    "example.net",
    ".invalid",
    "localhost",
    "127.0.0.1",
];

/// Расширения, которые считаются текстом. Всё остальное (png, ico, wasm)
/// не читается.
const TEXT_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "html", "js", "css", "json", "yml", "yaml", "txt", "sh",
];

fn workspace_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .expect("workspace Cargo.toml must be readable from the test");
    assert!(
        manifest.contains("[workspace]"),
        "{} is not the workspace root — the scan would silently cover nothing",
        root.display()
    );
    root
}

fn text_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => panic!("cannot read {}: {e}", dir.display()),
        };
        for entry in entries {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "target" || name == "node_modules" || name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| TEXT_EXTENSIONS.contains(&e))
            {
                out.push(path);
            }
        }
    }
    out
}

/// Класс 1: имя оператора где угодно в тексте.
fn forbidden_tokens_in(text: &str) -> Vec<(usize, &'static str)> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        for token in FORBIDDEN_TOKENS {
            if lower.contains(token) {
                hits.push((index + 1, *token));
            }
        }
    }
    hits
}

/// Класс 2: строковый литерал, присвоенный `rp_id` / `rp_origin` / `rp_name`.
fn rp_literals_in(text: &str) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for key in ["rp_id", "rp_origin", "rp_name"] {
            let mut from = 0usize;
            while let Some(at) = line[from..].find(key) {
                let start = from + at;
                let after = start + key.len();
                from = after;
                // Граница слова слева: иначе `corp_id: "test_corp"` читается
                // как `rp_id` — проверено, первый прогон стража покраснел
                // именно на нём.
                if line[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                {
                    continue;
                }
                let rest = line[after..].trim_start();
                let Some(rest) = rest
                    .strip_prefix(':')
                    .or_else(|| rest.strip_prefix('='))
                    .or_else(|| rest.strip_prefix('('))
                else {
                    continue;
                };
                let rest = rest.trim_start();
                let Some(rest) = rest.strip_prefix('"') else {
                    continue;
                };
                let Some(end) = rest.find('"') else {
                    continue;
                };
                let literal = &rest[..end];
                if !rp_literal_is_neutral(literal) {
                    hits.push((index + 1, literal.to_string()));
                }
            }
        }
    }
    hits
}

fn rp_literal_is_neutral(literal: &str) -> bool {
    let trimmed = literal.trim();
    if trimmed.is_empty() || NEUTRAL_RP_NAMES.contains(&trimmed) {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    RESERVED_HOST_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

#[test]
fn no_installation_specific_strings_in_sources_or_docs() {
    let root = workspace_root();
    let this_file = root.join("xtask/tests/no_installation_identifiers.rs");
    let mut failures = Vec::new();

    // Каталоги целиком плюс markdown в корне: README — ровно то место, где
    // имя заказчика оказывается первым.
    let mut scanned: Vec<PathBuf> = Vec::new();
    for area in ["crates", "xtask", "docs"] {
        let dir = root.join(area);
        assert!(dir.is_dir(), "{} must exist to be scanned", dir.display());
        scanned.extend(text_files(&dir));
    }
    let root_markdown: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("workspace root must be readable")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    assert!(
        root_markdown.iter().any(|p| p.ends_with("README.md")),
        "README.md must be among the scanned root markdown files"
    );
    scanned.extend(root_markdown);

    for file in scanned {
        if file == this_file {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue; // не-UTF-8 файл с текстовым расширением
        };
        let shown = file.strip_prefix(&root).unwrap_or(&file).display();
        for (line, token) in forbidden_tokens_in(&text) {
            failures.push(format!(
                "{shown}:{line}: имя конкретной установки {token:?}"
            ));
        }
        if file.extension().and_then(|e| e.to_str()) == Some("rs") {
            for (line, literal) in rp_literals_in(&text) {
                failures.push(format!(
                    "{shown}:{line}: rp_* = {literal:?} — нужен example.test / \
                     localhost / нейтральное имя (RFC 2606)"
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "в публичном форке остались строки конкретной установки:\n  {}",
        failures.join("\n  ")
    );
}

/// Страж бесполезен, если его матчер ничего не находит: тогда зелёное ничего
/// не значит. Здесь матчер проверяется на подсадном тексте — том самом,
/// который был в `webchat.rs`, и на законном упоминании, которое краснеть не
/// должно.
#[test]
fn the_matcher_itself_is_not_blind() {
    let planted = "<div class=\"eyebrow\">DenisAgency</div>\n\
                   rp_id: \"denis-openfang.moone.dev\".into(),\n";
    let tokens = forbidden_tokens_in(planted);
    assert_eq!(tokens.len(), 3, "expected 3 token hits, got {tokens:?}");
    assert_eq!(tokens[0], (1, "denisagency"));

    let literals = rp_literals_in(planted);
    assert_eq!(literals.len(), 1, "expected one rp_* hit, got {literals:?}");
    assert_eq!(literals[0].1, "denis-openfang.moone.dev");

    // Законные строки не краснеют.
    let innocent = "\"agents.none_fallback\": \"Нет — добавить цепочку резервов\",\n\
                    rp_name: \"OpenFang\".to_string(),\n\
                    rp_origin: \"https://openfang.example.test\".into(),\n\
                    corp_id: \"test_corp\".to_string(),\n\
                    let host = config.auth.rp_id.trim();\n\
                    \"auth.rp_origin must be an exact HTTPS origin\".into(),\n";
    assert!(forbidden_tokens_in(innocent).is_empty());
    assert!(
        rp_literals_in(innocent).is_empty(),
        "false positive: {:?}",
        rp_literals_in(innocent)
    );
}
