//! Ссылка на то, что мы сами переименовываем, обязана вести в существующее.
//!
//! # Почему этот файл есть
//!
//! Уборка ломает то, что на неё опиралось, и уже дважды подряд:
//!
//! 1. ветку `ours` удалили как дубликат `main` — `ofmutate` ветвился от неё и
//!    перестал работать;
//! 2. каталог скилла переименовали (`openfang` → `fang-upgrade`) — и **семь**
//!    отслеживаемых файлов остались указывать на каталог, которого нет. Среди
//!    них `scripts/ofrelease`: инструмент, которым катят прод.
//!
//! Общее у обоих случаев не «невнимательность», а отсутствие проверки: имя,
//! на которое ссылаются из файлов, менялось в одном месте и не менялось в
//! остальных, и узнать об этом было неоткуда до первого запуска.
//!
//! # Граница: какой путь эта проверка обязана видеть
//!
//! «Любой путь в любом файле» — граница нерабочая, и это измерено, а не
//! предположено: в дереве 48 абсолютных путей вида
//! `/var/lib/docker/volumes/<том>/_data/config.toml` и десятки относительных
//! вроде `agents/my-agent/agent.toml`. Первые — снятые логи: они записывают
//! состояние машины на момент прогона, и требовать от них существования значит
//! требовать переписать историю. Вторые — примеры в документации: файл, который
//! читатель создаёт сам, не существует ПО ЗАМЫСЛУ. Проверка, краснеющая на том
//! и на другом, будет отключена через неделю.
//!
//! Поэтому проверяются три вещи, и у каждой сказано, почему именно она:
//!
//! * **Правило 1 — имя каталога скилла.** Любое упоминание `SKILL_ROOT/<имя>`
//!   обязано называть [`SKILL_DIR`]. Это чистый текст: работает и на раннере
//!   GitHub, где никакого каталога скиллов нет. Ровно этот класс и был дефектом.
//! * **Правило 2 — путь внутрь скилла существует.** Работает только там, где
//!   [`SKILL_ROOT`] есть на диске (на раннере CI его нет — проверка это печатает,
//!   а не притворяется зелёной). Ловит переименование ВНУТРИ скилла: правило 1
//!   про `ofbackup` → `ofsnap` ничего не скажет, а это правило скажет.
//! * **Правило 3 — ссылка программы на файл этого репозитория.** Программа —
//!   файл, начинающийся с `#!`, плюс workflow в `.github/workflows`. Это форма,
//!   а не список: путь в программе — то, что она откроет, а не пример для
//!   читателя. Проза (`*.md`) не проверяется намеренно: отличить пример от
//!   ссылки в прозе формой нельзя, и попытка кончится отключённой проверкой.
//!   Внутри программы берутся только пути с расширением файла: `agents/models`
//!   в комментарии `A-4.sh` — это каталог внутри контейнера, а не файл репозитория.
//! * **Правило 4 — фильтр веток в workflow.** Имя ветки в триггере, которой нет,
//!   читается как покрытие, а покрытием не является: `fork-ci.yml` полгода
//!   назывался «Fork CI (ours)» и триггерился в том числе на `ours`.
//!
//! Чего проверка не делает: не ходит по путям контейнера (`/data`, `/build`) —
//! их на хосте нет по определению, и это не дефект; не проверяет URL; не читает
//! `.gitignore` (там шаблоны, а не пути).

use std::path::{Path, PathBuf};

/// Корень каталога скиллов агента.
const SKILL_ROOT: &str = "/root/.claude/skills";

/// Единственное имя каталога скилла, на которое этому репозиторию можно
/// ссылаться. Переименовали скилл — правится здесь, и красное держится ровно до
/// тех пор, пока не поправлены все файлы.
const SKILL_DIR: &str = "fang-upgrade";

/// Ветки, которые разрешено называть в триггерах workflow.
/// `main` — ствол. `upstream-sync-only` — намеренно несуществующая: апстримовы
/// `ci.yml` и `release.yml` сведены на ветку, куда никто не пушит, чтобы они не
/// запускались вовсе (см. шапки этих файлов). Ветки `ours` в списке нет: она
/// удалена 23 августа 2026 как дубликат `main`.
const DECLARED_BRANCHES: &[&str] = &["main", "upstream-sync-only"];

const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// Символы, из которых состоит путь. `*`, `<`, `$`, `{` в набор не входят:
/// на них разбор обрывается, поэтому шаблон `tests/fang/A-*.sh` и заглушка
/// `crates/<крейт>/src` в кандидаты не попадают вовсе.
fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'/' | b'@' | b'+' | b'~' | b'-')
}

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

fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

fn text_of(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Программа: файл с шебангом или workflow. Путь в ней — то, что она откроет.
fn is_program(root: &Path, path: &Path, text: &str) -> bool {
    if text.starts_with("#!") {
        return true;
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel = rel.to_string_lossy().replace('\\', "/");
    rel.starts_with(".github/workflows/")
}

/// Все токены-пути строки: `(токен, номер строки)`.
fn path_tokens(text: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if !is_path_byte(bytes[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && is_path_byte(bytes[i]) {
                i += 1;
            }
            // Слева отрезается то, чем путь начаться не может, но что состоит из
            // «путёвых» байтов: в `${OFBACKUP:-/root/.claude/skills/…}` разбор
            // иначе получает токен `-/root/…`, у которого первый сегмент `-root`,
            // и вся проверка молча проходит мимо. Ровно так и было: подсадка
            // устаревшего имени скилла в scripts/ofrelease осталась зелёной.
            // Точку слева не трогаем — с неё начинаются `./x` и `.github/x`.
            let token = line[start..i]
                .trim_start_matches(['-', '+', '@', '~', ','])
                .trim_end_matches(['.', ',', '-']);
            if token.contains('/') {
                out.push((token.to_owned(), index + 1));
            }
        }
    }
    out
}

/// Правило 1 + правило 3: чистый текст, работают в любом окружении.
fn textual_failures(root: &Path) -> Vec<String> {
    let tops: Vec<String> = std::fs::read_dir(root)
        .expect("корень дерева читается")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !SKIPPED_DIRS.contains(&n.as_str()))
        .collect();

    let mut failures = Vec::new();
    for file in all_files(root) {
        let Some(text) = text_of(&file) else {
            continue;
        };
        let shown = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string();
        let program = is_program(root, &file, &text);

        for (token, line) in path_tokens(&text) {
            // Правило 1: имя каталога скилла.
            if let Some(rest) = token.strip_prefix(&format!("{SKILL_ROOT}/")) {
                let named = rest.split('/').next().unwrap_or_default();
                if !named.is_empty() && named != SKILL_DIR {
                    failures.push(format!(
                        "{shown}:{line}: ссылка на каталог скилла '{named}', а есть только \
                         '{SKILL_DIR}' (правило 1)"
                    ));
                }
                continue;
            }
            if token.starts_with('/') || !program {
                continue;
            }
            // Правило 3: ссылка программы на файл репозитория.
            let token = token.strip_prefix("./").unwrap_or(&token);
            if token.contains("//") || token.contains("..") {
                continue;
            }
            let Some((first, _)) = token.split_once('/') else {
                continue;
            };
            if !tops.iter().any(|t| t == first) {
                continue;
            }
            let extension = Path::new(token)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            if extension.is_empty() || extension.len() > 6 {
                continue;
            }
            if !root.join(token).exists() {
                failures.push(format!(
                    "{shown}:{line}: программа ссылается на '{token}', которого в дереве нет \
                     (правило 3)"
                ));
            }
        }
    }
    failures.sort();
    failures.dedup();
    failures
}

/// Правило 2: путь внутрь скилла существует. Только там, где скилл есть.
fn skill_path_failures(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    for file in all_files(root) {
        let Some(text) = text_of(&file) else {
            continue;
        };
        let shown = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string();
        for (token, line) in path_tokens(&text) {
            if !token.starts_with(&format!("{SKILL_ROOT}/")) {
                continue;
            }
            if !Path::new(&token).exists() {
                failures.push(format!(
                    "{shown}:{line}: '{token}' не существует на этой машине (правило 2)"
                ));
            }
        }
    }
    failures.sort();
    failures.dedup();
    failures
}

/// Правило 4: ветки в триггерах workflow.
fn branch_filter_failures(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let workflows = root.join(".github").join("workflows");
    let Ok(entries) = std::fs::read_dir(&workflows) else {
        return failures;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(text) = text_of(&path) else {
            continue;
        };
        let shown = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let Some((_, list)) = line.split_once("branches:") else {
                continue;
            };
            // YAML пишет список двумя способами. Инлайн — `branches: [main]`.
            // Блоком — `branches:` и дальше строки `- main`. Разбирать только
            // первый значило бы завести правило с тихой дырой ровно того вида,
            // против которого оно и написано.
            let mut names: Vec<(String, usize)> = list
                .split(['[', ']', ',', '"', '\'', ' '])
                .map(str::trim)
                .filter(|n| !n.is_empty() && *n != "-")
                .map(|n| (n.to_owned(), index + 1))
                .collect();
            if list.trim().is_empty() {
                for (offset, next) in lines.iter().enumerate().skip(index + 1) {
                    let trimmed = next.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    let Some(item) = trimmed.strip_prefix("- ") else {
                        break;
                    };
                    let item = item.trim().trim_matches(['"', '\'']);
                    if item.is_empty() {
                        break;
                    }
                    names.push((item.to_owned(), offset + 1));
                }
            }
            for (name, at) in names {
                if !DECLARED_BRANCHES.contains(&name.as_str()) {
                    failures.push(format!(
                        "{shown}:{at}: триггер называет ветку '{name}', которой нет в списке \
                         объявленных (правило 4)"
                    ));
                }
            }
        }
    }
    failures.sort();
    failures
}

// ── проверки ────────────────────────────────────────────────────────────────

#[test]
fn every_reference_this_repo_owns_resolves() {
    let root = workspace_root();
    let mut failures = textual_failures(&root);
    failures.extend(branch_filter_failures(&root));

    if Path::new(SKILL_ROOT).is_dir() {
        failures.extend(skill_path_failures(&root));
    } else {
        println!(
            "{SKILL_ROOT} на этой машине отсутствует (так на раннере CI): правило 2 \
             пропущено, правило 1 отработало по тексту"
        );
    }

    assert!(
        failures.is_empty(),
        "ссылки ведут в несуществующее:\n  {}",
        failures.join("\n  ")
    );
}

/// Проверка, которая не может покраснеть, — не проверка. Здесь ей подсаживают
/// ровно те три поломки, ради которых она заведена, на дереве во временном
/// каталоге: настоящее дерево при этом остаётся нетронутым.
#[test]
fn the_check_goes_red_on_each_kind_of_dead_reference() {
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = std::env::temp_dir().join(format!("ofrefs-{stamp}"));
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::create_dir_all(root.join(".github").join("workflows")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs").join("real.md"), "живой файл\n").unwrap();

    // Живое дерево: ссылки ведут в существующее.
    std::fs::write(
        root.join("scripts").join("live.sh"),
        format!(
            "#!/bin/sh\n. docs/real.md\nOFBACKUP={SKILL_ROOT}/{SKILL_DIR}/scripts/ofbackup\n\
             # шаблон в комментарии программы: tests/fang/A-*.sh — разбор обрывается\n\
             # на звёздочке, поэтому в кандидаты он не попадает.\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join(".github").join("workflows").join("ci.yml"),
        "on:\n  push:\n    branches: [main]\n",
    )
    .unwrap();
    // Проза с примером, которого нет: краснеть не должна.
    std::fs::write(
        root.join("docs").join("guide.md"),
        "создайте `agents/my-agent/agent.toml` и запустите\n",
    )
    .unwrap();

    let clean = {
        let mut f = textual_failures(&root);
        f.extend(branch_filter_failures(&root));
        f
    };
    assert!(clean.is_empty(), "живое дерево покраснело: {clean:?}");

    // Подсадка 1: устаревшее имя каталога скилла (правило 1). Форма взята из
    // scripts/ofrelease — значение по умолчанию в подстановке `${VAR:-…}`:
    // именно на ней первая редакция этой проверки молчала, потому что дефис
    // перед слэшем уезжал в токен.
    let stale = format!("{SKILL_ROOT}/openfang/scripts/ofgate");
    std::fs::write(
        root.join("scripts").join("stale.sh"),
        format!("#!/bin/sh\nOFGATE=\"${{OFGATE:-{stale}}}\"\nexec \"$OFGATE\" \"$@\"\n"),
    )
    .unwrap();
    // Подсадка 2: программа ссылается на файл репозитория, которого нет (правило 3).
    std::fs::write(
        root.join("scripts").join("broken.sh"),
        "#!/bin/sh\ncat docs/gone.md\n",
    )
    .unwrap();
    // Подсадка 3: триггер называет удалённую ветку (правило 4).
    std::fs::write(
        root.join(".github").join("workflows").join("stale.yml"),
        "on:\n  push:\n    branches: [main, ours]\n",
    )
    .unwrap();
    // ...и та же ветка, записанная блоком: инлайн-разбор её не видит.
    std::fs::write(
        root.join(".github").join("workflows").join("block.yml"),
        "on:\n  push:\n    branches:\n      - main\n      - ours\n",
    )
    .unwrap();

    let red = {
        let mut f = textual_failures(&root);
        f.extend(branch_filter_failures(&root));
        f
    };
    assert!(
        red.iter()
            .any(|f| f.contains("scripts/stale.sh") && f.contains("правило 1")),
        "правило 1 промолчало: {red:?}"
    );
    assert!(
        red.iter()
            .any(|f| f.contains("scripts/broken.sh") && f.contains("правило 3")),
        "правило 3 промолчало: {red:?}"
    );
    assert!(
        red.iter()
            .any(|f| f.contains("stale.yml") && f.contains("правило 4")),
        "правило 4 промолчало на инлайн-списке: {red:?}"
    );
    assert!(
        red.iter()
            .any(|f| f.contains("block.yml:5") && f.contains("правило 4")),
        "правило 4 промолчало на списке блоком: {red:?}"
    );
    assert!(
        !red.iter().any(|f| f.contains("docs/guide.md")),
        "пример в прозе покраснел: {red:?}"
    );

    // Правило 2 на этом же дереве: путь внутрь скилла, которого нет.
    std::fs::write(
        root.join("scripts").join("inside.sh"),
        format!(
            "#!/bin/sh\nOF=\"${{OF:-{SKILL_ROOT}/{SKILL_DIR}/scripts/ofnothing}}\"\nexec \"$OF\"\n"
        ),
    )
    .unwrap();
    if Path::new(SKILL_ROOT).is_dir() {
        let inside = skill_path_failures(&root);
        assert!(
            inside.iter().any(|f| f.contains("ofnothing")),
            "правило 2 промолчало: {inside:?}"
        );
    }

    std::fs::remove_dir_all(&root).unwrap();
}

/// Список объявленных веток обязан оставаться коротким и осмысленным: пустой
/// или разросшийся список превращает правило 4 в украшение.
#[test]
fn the_declared_branch_list_stays_small() {
    assert!(
        DECLARED_BRANCHES.len() <= 3,
        "объявленных веток стало {} — список перестал что-либо запрещать",
        DECLARED_BRANCHES.len()
    );
    assert!(
        DECLARED_BRANCHES.contains(&"main"),
        "ствол обязан быть в списке"
    );
    assert!(
        !DECLARED_BRANCHES.contains(&"ours"),
        "ветка ours удалена 23 августа 2026; вернуть её в список можно только вместе с веткой"
    );
}
