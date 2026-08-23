//! Страж имени установки.
//!
//! Форк публичный: его исходники читают и поднимают у себя чужие люди. Строка,
//! называющая конкретную установку — её бренд, её домен, её тайлнет-адрес, её
//! WebAuthn-личность, — попав в исходник, уезжает к каждому, кто соберёт этот
//! код. Так и случилось: страница входа несла бренд оператора, фикстуры тестов —
//! живой домен, а `docker-compose.dokploy.yml` — имя traefik-мидлвари и адрес
//! прода. Одной вычистки мало, потому что следующая правка занесёт заново;
//! поэтому запрет живёт тестом, а не памятью.
//!
//! # Страж не содержит того, что ищет
//!
//! Предыдущая версия хранила имена открытым текстом и исключала себя из обхода.
//! Итогом обезличивания было не ноль вхождений имени в публичном репозитории, а
//! тринадцать — и десять из них лежали ровно в том файле, который заведён это
//! имя не пускать. Здесь имён нет: хранятся только длина токена и два его хэша
//! (FNV-1a 64 для быстрого просеивания, SHA-256 для подтверждения). Обход
//! сравнивает хэши окон, а не строки, и **этот файл из обхода не исключён**.
//!
//! Проверить, что конкретное имя покрыто, может любой, кто это имя знает, —
//! стандартной командой, не заглядывая в код:
//!
//! ```text
//! $ printf '%s' "<имя в нижнем регистре>" | sha256sum
//! ```
//!
//! и сравнить с колонкой `sha256` в [`FORBIDDEN_TOKEN_HASHES`]. Обратно —
//! из таблицы имя не читается. Тот же вопрос задаётся и прогоном, без
//! записи имени в файл:
//!
//! ```text
//! $ OFGUARD_PROBE='<имя>' cargo test -p xtask --test no_installation_identifiers \
//!       -- --ignored --nocapture probe_token_coverage
//! ```
//!
//! # Что именно ловится
//!
//! 1. **Имена оператора.** Закрытый список токенов (по хэшам), сравнение
//!    регистронезависимое, совпадение ищется в любом месте строки. Списком, а не
//!    эвристикой: слово «резерв» встречается в русской локали
//!    (`agents.none_fallback`) как обычное слово, и общий поиск по именам людей
//!    покраснел бы на нём.
//! 2. **WebAuthn relying party.** `rp_id`, `rp_origin` и `rp_name` — это и есть
//!    личность установки по определению: RP ID обязан совпадать с публичным
//!    доменом. Правило: если за ключом (с необязательной закрывающей кавычкой,
//!    как в JSON) идёт `:`, `=` или `(`, а за ними — **строковый литерал в
//!    кавычках**, то этот литерал обязан быть либо пустым, либо зарезервированным
//!    именем (RFC 2606/6761), либо нейтральным названием продукта. Литерал
//!    ищется через необязательную обёртку из идентификаторов и скобок
//!    (`String::from("…")`, `Some("…")`), через перевод строки и в форме
//!    raw-строки (`r"…"`, `r#"…"#`). Значение **без кавычек** (голый скаляр
//!    YAML/TOML, `${VAR}`, вызов функции) этим правилом не разбирается — такие
//!    случаи ловятся только классом 1.
//!
//! # Что обходится
//!
//! Всё рабочее дерево, кроме `.git`, `target` и `node_modules`. Не по списку
//! расширений: разбор подсадил `.py` внутрь `crates/` и прошёл мимо списка.
//! Читается любой файл, который целиком разбирается как UTF-8 и не содержит
//! нулевого байта; остальное (png, ico, wasm) пропускается.
//!
//! # Что печатается при падении
//!
//! Путь и номер строки, номер токена в таблице — и ничего из содержимого.
//! Логи Actions публичного репозитория публичны, и страж, печатающий найденное
//! имя, вернул бы утечку через свой же отчёт.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Имена конкретной установки: `(длина в байтах, FNV-1a 64, SHA-256 hex)`
/// от токена в нижнем регистре.
///
/// Пополняется так (имя не попадает ни в файл, ни в историю шелла — ведущий
/// пробел не пишется в `~/.bash_history` при `HISTCONTROL=ignorespace`):
///
/// ```text
///  $ printf '%s' "<имя>" | sha256sum
/// ```
///
/// FNV-1a считает `fnv1a64`: он в этом же файле и покрыт тестом
/// `fnv_matches_its_published_vector`.
const FORBIDDEN_TOKEN_HASHES: &[(usize, u64, &str)] = &[
    (
        11,
        0xe90c_cfd0_a795_06eb,
        "24e8e0cb761028670185cb8a3b527ead0da0a4e76db9ab5ade6068be7913fccc",
    ),
    (
        14,
        0x3546_8d30_7c73_2fa5,
        "bcab5c096ab43b29a840e35251d079d1245149878317c54202d162a79c41dcfa",
    ),
    (
        9,
        0xba13_4e61_75a8_dc22,
        "637a2faae1b6c2dbaa4a44254ec3e53565f7eb65c83e5872b355234f594ad2f2",
    ),
    (
        13,
        0x25a7_81df_370e_7a24,
        "cede268ef9e2c40ab9cade1624dada397b784f29748d6fe1d778674cb5ecdc1f",
    ),
];

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

/// Каталоги, которых в обходе нет ни при каких условиях.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

// ── хэши ────────────────────────────────────────────────────────────────────

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Номер токена в [`FORBIDDEN_TOKEN_HASHES`], если байты — это он.
/// FNV просеивает, SHA-256 подтверждает: на FNV полагаться как на
/// единственное сравнение нельзя, потому что 64 бита без ключа
/// подбираются.
fn token_index(window: &[u8]) -> Option<usize> {
    for (index, (len, fnv, sha)) in FORBIDDEN_TOKEN_HASHES.iter().enumerate() {
        if window.len() == *len && fnv1a64(window) == *fnv && &sha256_hex(window) == sha {
            return Some(index);
        }
    }
    None
}

// ── класс 1: имена оператора ────────────────────────────────────────────────

/// Возвращает `(номер строки, номер токена)` — без единого байта найденного.
fn forbidden_tokens_in(text: &str) -> Vec<(usize, usize)> {
    let mut lengths: Vec<usize> = FORBIDDEN_TOKEN_HASHES
        .iter()
        .map(|(len, _, _)| *len)
        .collect();
    lengths.sort_unstable();
    lengths.dedup();

    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        for len in &lengths {
            if bytes.len() < *len {
                continue;
            }
            for window in bytes.windows(*len) {
                if let Some(token) = token_index(window) {
                    hits.push((index + 1, token));
                }
            }
        }
    }
    hits.sort_unstable();
    hits.dedup();
    hits
}

// ── класс 2: строковый литерал `rp_*` ───────────────────────────────────────

const RP_KEYS: &[&str] = &["rp_id", "rp_origin", "rp_name"];

/// Сколько байтов после разделителя разрешено пройти в поисках литерала.
/// Ограничение есть, чтобы объявление поля (`pub rp_id: String,`) не утягивало
/// разбор в следующие строки файла.
const RP_LOOKAHEAD: usize = 200;

fn is_wrapper_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(byte, b'_' | b':' | b'<' | b'>' | b'&' | b'*' | b'(')
        || byte.is_ascii_whitespace()
}

/// Строковый литерал сразу за `at`, если он там есть: обычный `"…"` или
/// raw-строка `r"…"` / `r#"…"#`. Возвращает `(литерал, смещение начала)`.
fn string_literal_at(bytes: &[u8], at: usize) -> Option<(String, usize)> {
    if at >= bytes.len() {
        return None;
    }
    if bytes[at] == b'"' {
        let mut i = at + 1;
        let mut out = Vec::new();
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    if let Some(next) = bytes.get(i + 1) {
                        out.push(*next);
                    }
                    i += 2;
                }
                b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), at)),
                b'\n' => return None,
                other => {
                    out.push(other);
                    i += 1;
                }
            }
        }
        return None;
    }
    if bytes[at] == b'r' {
        let mut hashes = 0usize;
        let mut i = at + 1;
        while bytes.get(i) == Some(&b'#') {
            hashes += 1;
            i += 1;
        }
        if bytes.get(i) != Some(&b'"') {
            return None;
        }
        let start = i + 1;
        let mut close = Vec::with_capacity(hashes + 1);
        close.push(b'"');
        close.extend(std::iter::repeat_n(b'#', hashes));
        let mut j = start;
        while j + close.len() <= bytes.len() {
            if &bytes[j..j + close.len()] == close.as_slice() {
                return Some((String::from_utf8_lossy(&bytes[start..j]).into_owned(), at));
            }
            j += 1;
        }
        return None;
    }
    None
}

/// Класс 2: строковый литерал, присвоенный `rp_id` / `rp_origin` / `rp_name`.
fn rp_literals_in(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();

    for key in RP_KEYS {
        let mut from = 0usize;
        while let Some(at) = text[from..].find(key) {
            let start = from + at;
            let after = start + key.len();
            from = after;

            // Граница слова слева: иначе `corp_id: "test_corp"` читается как
            // `rp_id` — проверено, первый прогон стража покраснел именно на нём.
            if text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                continue;
            }
            // ...и справа: `rp_ids`, `rp_name_source` — другие имена.
            if bytes
                .get(after)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                continue;
            }

            let mut i = after;
            // Ключ мог быть в кавычках — форма JSON. Снимаем закрывающую.
            if matches!(bytes.get(i), Some(b'"') | Some(b'\'')) {
                i += 1;
            }
            while matches!(bytes.get(i), Some(b' ') | Some(b'\t')) {
                i += 1;
            }
            // Разделитель. `::` — это путь, `==` — сравнение, ни то ни другое
            // не присваивание.
            match bytes.get(i) {
                Some(b':') if bytes.get(i + 1) != Some(&b':') => i += 1,
                Some(b'=') if bytes.get(i + 1) != Some(&b'=') => i += 1,
                Some(b'(') => i += 1,
                _ => continue,
            }

            // Обёртка: `String::from(`, `Some(`, `&`, перевод строки.
            let limit = (i + RP_LOOKAHEAD).min(bytes.len());
            let found = loop {
                if i >= limit {
                    break None;
                }
                if let Some(hit) = string_literal_at(bytes, i) {
                    break Some(hit);
                }
                if is_wrapper_byte(bytes[i]) {
                    i += 1;
                } else {
                    break None;
                }
            };
            let Some((literal, offset)) = found else {
                continue;
            };
            if !rp_literal_is_neutral(&literal) {
                hits.push((line_of(text, offset), literal));
            }
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

fn line_of(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset.min(text.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1
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

// ── обход дерева ────────────────────────────────────────────────────────────

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

/// Все файлы дерева, кроме [`SKIPPED_DIRS`]. Символические ссылки не
/// разыменовываются: цикл ссылок повесил бы обход.
fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => panic!("cannot read {}: {e}", dir.display()),
        };
        for entry in entries {
            let entry = entry.expect("directory entry");
            let name = entry.file_name();
            let name = name.to_string_lossy().into_owned();
            if SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            let file_type = entry.file_type().expect("file type");
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

/// Текст файла, если он читается как UTF-8 и не двоичный.
fn text_of(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

// ── сами проверки ───────────────────────────────────────────────────────────

#[test]
fn no_installation_specific_strings_anywhere_in_the_tree() {
    let root = workspace_root();
    let files = all_files(&root);
    assert!(
        files.len() > 200,
        "обход вернул {} файлов — дерево так не выглядит, значит обход сломан",
        files.len()
    );
    // Каталоги, попадание которых в обход и было дефектом: корень (не только
    // *.md), `.github`, `scripts`, `deploy`, `tests`. Проверяем, что они там.
    for must in [
        "docker-compose.dokploy.yml",
        "openfang.toml.example",
        ".github/workflows/fork-ci.yml",
        "README.md",
        "xtask/tests/no_installation_identifiers.rs",
    ] {
        let path = root.join(must);
        assert!(
            files.contains(&path),
            "{must} обязан быть в обходе — именно мимо таких файлов страж и ходил"
        );
    }

    let mut failures = Vec::new();
    let mut read = 0usize;
    for file in &files {
        let Some(text) = text_of(file) else {
            continue;
        };
        read += 1;
        let shown = file.strip_prefix(&root).unwrap_or(file).display();
        for (line, token) in forbidden_tokens_in(&text) {
            failures.push(format!("{shown}:{line}: имя установки, токен #{token}"));
        }
        for (line, literal) in rp_literals_in(&text) {
            failures.push(format!(
                "{shown}:{line}: rp_* — литерал длиной {} символов; нужен \
                 example.test / localhost / нейтральное имя (RFC 2606). \
                 Значение не печатается: логи Actions публичны",
                literal.chars().count()
            ));
        }
    }
    assert!(
        read > 200,
        "прочитано всего {read} текстовых файлов — сниффер съел дерево"
    );

    assert!(
        failures.is_empty(),
        "в публичном форке остались строки конкретной установки:\n  {}",
        failures.join("\n  ")
    );
}

/// Страж бесполезен, если его матчер ничего не находит: тогда зелёное ничего не
/// значит. Матчер проверяется на канарейке — на токене, которого в таблице
/// установки нет, — потому что подсадить сюда настоящее имя значило бы вернуть
/// ровно тот дефект, ради которого таблица стала хэшами.
#[test]
fn the_matcher_itself_is_not_blind() {
    const CANARY: &str = "adv-canary-9f31";
    const CANARY_FNV: u64 = 0x62dc_d952_ca07_7feb;
    const CANARY_SHA: &str = "442db2a48908bde917cf5974464c3d64d2de1d047c8cdf6c50252132b64577a6";

    // Тот же матчер, но по таблице из одной канарейки.
    let find = |text: &str| -> Vec<usize> {
        let mut out = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let lower = line.to_ascii_lowercase();
            for window in lower.as_bytes().windows(CANARY.len()) {
                if fnv1a64(window) == CANARY_FNV && sha256_hex(window) == CANARY_SHA {
                    out.push(index + 1);
                }
            }
        }
        out
    };

    assert_eq!(
        find(&format!("<div class=\"eyebrow\">{CANARY}</div>")),
        vec![1]
    );
    // Регистронезависимо и в середине слова.
    assert_eq!(
        find(&format!(
            "x\nhttps://{}.example.test/",
            CANARY.to_uppercase()
        )),
        vec![2]
    );
    assert!(find("ничего похожего тут нет").is_empty());
    // Соседний токен не совпадает: хэш, а не префикс.
    assert!(find("adv-canary-9f30").is_empty());

    // Хэш-функция в этом файле — та же, что считала таблицу.
    assert_eq!(fnv1a64(CANARY.as_bytes()), CANARY_FNV);
    assert_eq!(sha256_hex(CANARY.as_bytes()), CANARY_SHA);
}

/// Таблица токенов — единственное, чем страж отличает имя установки от любого
/// другого текста. Пустая или битая таблица делает обход зелёным всегда.
#[test]
fn the_token_table_is_well_formed() {
    assert!(
        FORBIDDEN_TOKEN_HASHES.len() >= 4,
        "таблица усохла до {} записей",
        FORBIDDEN_TOKEN_HASHES.len()
    );
    for (index, (len, fnv, sha)) in FORBIDDEN_TOKEN_HASHES.iter().enumerate() {
        assert!(
            *len >= 6,
            "токен #{index}: длина {len} — слишком коротко, будет ловить случайный текст"
        );
        assert_ne!(*fnv, 0, "токен #{index}: пустой FNV");
        assert_eq!(sha.len(), 64, "токен #{index}: SHA-256 не 64 знака");
        assert!(
            sha.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "токен #{index}: SHA-256 не hex в нижнем регистре"
        );
    }
    // Ни один токен не должен совпасть с безобидным текстом той же длины.
    assert!(token_index(b"example.test").is_none());
    assert!(token_index(b"openfang").is_none());
}

/// FNV-1a здесь свой, без зависимости. Опубликованный вектор из спецификации
/// доказывает, что это именно FNV-1a 64, а не «похожая» петля: пересчитать
/// таблицу сторонним инструментом можно только зная это.
#[test]
fn fnv_matches_its_published_vector() {
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
}

/// Правило `rp_*` обязано покрывать те формы, которыми его обходили: обёртку
/// `String::from`, raw-строку и литерал на следующей строке. Все три —
/// воспроизведённые атаки, а не гипотезы.
#[test]
fn rp_rule_covers_the_forms_that_slipped_past_it() {
    // Ключ и хвост держатся ПОРОЗНЬ и склеиваются в рантайме. Иначе фикстура
    // краснела бы на собственном обходе: этот файл из обхода не исключён, а
    // ключ, двоеточие и литерал одной строкой — ровно то, что правило и ловит.
    let caught: &[(&str, &str)] = &[
        ("rp_id", ": \"probe-a.net\","),
        ("rp_id", ": String::from(\"probe-b.net\"),"),
        ("rp_origin", ": r#\"https://probe-c.net\"#,"),
        ("rp_name", ":\n    \"Probe Agency\","),
        ("rp_id", " = \"probe-e.net\""),
        ("rp_id", "\": \"probe-f.net\""),
        ("rp_origin", ": Some(\"https://probe-g.net\")"),
        ("rp_id", ": &\"probe-h.net\""),
    ];
    for (key, tail) in caught {
        let case = format!("{key}{tail}");
        assert_eq!(
            rp_literals_in(&case).len(),
            1,
            "форма прошла мимо правила: {case:?} -> {:?}",
            rp_literals_in(&case)
        );
    }

    let innocent = [
        r#""agents.none_fallback": "Нет — добавить цепочку резервов","#,
        r#"rp_name: "OpenFang".to_string(),"#,
        r#"rp_origin: "https://openfang.example.test".into(),"#,
        r#"corp_id: "test_corp".to_string(),"#,
        "let host = config.auth.rp_id.trim();",
        r#""auth.rp_origin must be an exact HTTPS origin".into(),"#,
        "pub rp_id: String,\n    pub other: &'static str = \"anything\",",
        r#"rp_name: format!("{brand} Ltd"),"#,
        r#"if cfg.rp_id == "example.test" { }"#,
        r#"rp_id: "","#,
    ];
    for case in innocent {
        assert!(
            rp_literals_in(case).is_empty(),
            "ложное срабатывание на {case:?}: {:?}",
            rp_literals_in(case)
        );
    }
}

/// Ответ на вопрос «покрыто ли имя X» без записи X в репозиторий.
/// Запускается вручную:
/// `OFGUARD_PROBE='<имя>' cargo test -p xtask --test no_installation_identifiers
///  -- --ignored --nocapture probe_token_coverage`
#[test]
#[ignore = "ручная проба: имя приходит через OFGUARD_PROBE"]
fn probe_token_coverage() {
    let probe =
        std::env::var("OFGUARD_PROBE").expect("OFGUARD_PROBE не задан: пробе нечего проверять");
    let lower = probe.to_ascii_lowercase();
    match token_index(lower.as_bytes()) {
        Some(index) => println!("OFGUARD_PROBE: покрыт, токен #{index}"),
        None => println!("OFGUARD_PROBE: НЕ покрыт (длина {})", lower.len()),
    }
}
