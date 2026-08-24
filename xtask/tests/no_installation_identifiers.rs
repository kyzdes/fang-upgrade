//! Страж значений, привязанных к одной установке.
//!
//! Форк публичный: его исходники читают и поднимают у себя чужие люди. Значение,
//! называющее конкретную установку — её домен, её адрес, её WebAuthn-личность, —
//! попав в исходник, уезжает к каждому, кто соберёт этот код. Так уже случалось:
//! страница входа несла бренд оператора, фикстуры тестов — живой домен, а
//! `docker-compose.dokploy.yml` — адрес прода. Одной вычистки мало, потому что
//! следующая правка занесёт заново; поэтому запрет живёт тестом, а не памятью.
//!
//! # Почему здесь нет ни имён, ни их хэшей
//!
//! Две предыдущие редакции пытались «обнаружить строку, не содержа её». Первая
//! хранила имена открытым текстом и исключала себя из обхода: пересчёт по
//! дереву на `63e7c16^` даёт 17 вхождений имён установки в отслеживаемых
//! файлах, из них 10 — в самом страже. Вторая заменила имена на тройки
//! `(длина, FNV-1a, SHA-256)` и утверждала в докстринге, что «обратно из
//! таблицы имя не читается».
//!
//! **Утверждение ложное, и это пересчитано, а не предположено:** sha256 от
//! четырёх коротких угадываемых имён совпал со всеми четырьмя строками той
//! таблицы; ведущий отдельно восстановил их словарём из 1603 кандидатов за долю
//! секунды. Несолёный хэш от короткой угадываемой строки с ОПУБЛИКОВАННОЙ
//! длиной — учебниковый перебор, а секретной соли в публичном файле не бывает.
//! Такая таблица хуже открытого текста: она создаёт видимость защиты.
//!
//! Поэтому подход сменён: **страж проверяет форму значения, а не список имён.**
//! Ему нечего скрывать, он не исключает себя из обхода, и всё, что в нём
//! написано открытым текстом, — это РАЗРЕШЁННЫЕ значения.
//!
//! # Форма: что считается привязанным к установке
//!
//! ## Класс 1. Литерал адреса хоста
//!
//! Адрес-литерал называет одну машину. Диапазон (запись с длиной префикса,
//! `10.0.0.0/8`, `fd7a:115c:a1e0::/48`) называет КЛАСС адресов и машины не
//! называет — поэтому диапазоны пропускаются, а host-литералы разбираются.
//! Проверка идёт двумя порогами:
//!
//! * **везде** красное, если адрес маршрутизируется в публичной сети. Не
//!   маршрутизируются и молча пропускаются: `0.0.0.0/8`, `10/8`, `100.64/10`
//!   (RFC 6598), `127/8`, `169.254/16`, `172.16/12`, `192.168/16`, диапазоны
//!   документации RFC 5737 (`192.0.2/24`, `198.51.100/24`, `203.0.113/24`),
//!   `224/4`, `240/4`; в IPv6 — `::/8` (включая `::` и `::1`), ULA `fc00::/7`,
//!   link-local `fe80::/10`, документация `2001:db8::/32` (RFC 3849), `ff00::/8`;
//! * **в файле конфигурации** (`*.toml`, `*.yml`, `*.yaml`, `*.json`, `*.ini`,
//!   `*.conf`, `*.env`/`.env*`, `*.service`, `*.nix`, `*.example`, `Dockerfile*`,
//!   `docker-compose*`) порог выше: там законны только loopback, `0.0.0.0` и
//!   диапазоны документации. Приватный адрес в опубликованном конфиге — это
//!   привязка к чьей-то одной сети; ровно так адрес прода и уехал в
//!   `docker-compose.dokploy.yml`.
//!
//! Два вида ложных срабатываний сняты формой, а не списком: `Chrome/131.0.0.0`
//! — это версия (четвёрка, приклеенная одним `/` к имени продукта, перед которым
//! нет слэша), а `${x:0:2}` и случайные байты в png дают две шестнадцатеричные
//! группы вокруг `::`. Поэтому сокращённая запись IPv6 разбирается только от
//! трёх групп. Цена сказана прямо: адрес, уместившийся в две группы, этой
//! проверкой не ловится, и тайлнет-адрес установки не ловится тоже — тайлнет
//! живёт в приватном пространстве (`100.64/10`, ULA), которое законно
//! встречается в коде FANG-95 и в тестах, и по виду одно от другого не
//! отличается.
//!
//! ## Класс 2. Личность WebAuthn (`rp_*`)
//!
//! `rp_id`, `rp_origin` и `rp_name` — это и есть личность установки по
//! определению: RP ID обязан совпадать с публичным доменом. Если за ключом (с
//! необязательной закрывающей кавычкой, как в JSON) идёт `:`, `=` или `(`, то
//! значение обязано быть либо пустым, либо зарезервированным именем (RFC
//! 2606/6761), либо нейтральным названием продукта. Разбираются: `"…"`, `'…'`
//! (литеральная строка TOML, скаляр YAML, Python, sh), raw-строка `r"…"` /
//! `r#"…"#`, обёртка из идентификаторов и скобок (`String::from("…")`,
//! `Some("…")`, `&"…"`), перенос значения на следующую строку — и, **только в
//! файле конфигурации**, скаляр без кавычек (`rp_id = dash.acme.tld`). Без
//! кавычек и не в конфиге разбора нет намеренно: в Rust `rp_id: cfg.auth.host`
//! — это выражение, а не значение, и общий разбор красил бы такие строки.
//!
//! Чего класс 2 не видит и видеть не может: значение, собранное в рантайме
//! (`format!`, переменная окружения, чтение файла). Текстовый страж читает
//! текст.
//!
//! # Чего страж не ловит вообще
//!
//! **Бренд.** Слово вроде «Acme» ничем по форме не отличается от любого другого
//! слова; поймать его можно только списком имён, а список имён в публичном
//! репозитории ничего не прячет. Поэтому здесь такого правила нет, и обещания
//! тоже нет. Бренд оператора держится из конфигурации (`auth.rp_name`), и это
//! свойство защищено тестами в `crates/openfang-api`, а не этим файлом.
//!
//! # Что обходится
//!
//! Всё рабочее дерево, кроме `.git`, `target` и `node_modules`. Не по списку
//! расширений: файл без расширения, вложенный каталог, двоичный файл — всё
//! читается. Не-UTF-8 читается как байты (`from_utf8_lossy`): формы, которые
//! ищет страж, целиком в ASCII, и пропускать такой файл значило бы оставить
//! дыру шириной в один `iconv`. Символическая ссылка не разыменовывается (цикл
//! ссылок повесил бы обход), но её ЦЕЛЬ разбирается как текст: путь, в котором
//! записан адрес установки, — такая же утечка, как строка в файле.
//!
//! # Что печатается при падении
//!
//! Путь, номер строки, форма и длина найденного — и ни одного его байта. Логи
//! Actions публичного репозитория публичны, и страж, печатающий найденное
//! значение, вернул бы утечку через свой же отчёт.

use std::path::{Path, PathBuf};

// ── что законно ─────────────────────────────────────────────────────────────

/// `(сеть, длина префикса)` — IPv4, не маршрутизируемые в публичной сети.
const V4_NOT_ROUTABLE: &[([u8; 4], u32)] = &[
    ([0, 0, 0, 0], 8),       // «этот хост», RFC 1122
    ([10, 0, 0, 0], 8),      // RFC 1918
    ([100, 64, 0, 0], 10),   // RFC 6598, shared address space (CGNAT, тайлнет)
    ([127, 0, 0, 0], 8),     // loopback
    ([169, 254, 0, 0], 16),  // link-local, включая IMDS 169.254.169.254
    ([172, 16, 0, 0], 12),   // RFC 1918
    ([192, 168, 0, 0], 16),  // RFC 1918
    ([192, 0, 2, 0], 24),    // RFC 5737, TEST-NET-1
    ([198, 51, 100, 0], 24), // RFC 5737, TEST-NET-2
    ([203, 0, 113, 0], 24),  // RFC 5737, TEST-NET-3
    ([224, 0, 0, 0], 4),     // multicast
    ([240, 0, 0, 0], 4),     // reserved + 255.255.255.255
];

/// IPv4, допустимые в файле конфигурации: только заглушки и loopback.
const V4_IN_CONFIG: &[([u8; 4], u32)] = &[
    ([0, 0, 0, 0], 8),
    ([127, 0, 0, 0], 8),
    ([192, 0, 2, 0], 24),
    ([198, 51, 100, 0], 24),
    ([203, 0, 113, 0], 24),
];

/// `(сеть, длина префикса)` — IPv6, не маршрутизируемые в публичной сети.
const V6_NOT_ROUTABLE: &[([u16; 8], u32)] = &[
    ([0, 0, 0, 0, 0, 0, 0, 0], 8), // ::/8 — резерв IETF, сюда попадают :: и ::1
    ([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7), // ULA, RFC 4193 (в нём живёт тайлнет)
    ([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10), // link-local
    ([0x2001, 0x0db8, 0, 0, 0, 0, 0, 0], 32), // RFC 3849, документация
    ([0xff00, 0, 0, 0, 0, 0, 0, 0], 8), // multicast
];

/// IPv6, допустимые в файле конфигурации.
const V6_IN_CONFIG: &[([u16; 8], u32)] = &[
    ([0, 0, 0, 0, 0, 0, 0, 0], 8),
    ([0x2001, 0x0db8, 0, 0, 0, 0, 0, 0], 32),
];

/// Адреса-исключения: маршрутизируемые литералы, которые не могут быть ничьей
/// установкой. Это не список «наших» значений — наоборот, список ЧУЖИХ констант,
/// и каждая строка обязана называть, чья она и зачем лежит в дереве. Новый
/// публичный адрес краснит гейт до тех пор, пока кто-нибудь не напишет здесь
/// такую же строку — это и есть цена правила, и она взята сознательно.
const EXEMPT_ADDRESSES: &[(&str, &str)] = &[
    (
        "8.8.8.8",
        "Google Public DNS — пример «не приватного» адреса в тестах SSRF",
    ),
    ("1.1.1.1", "Cloudflare DNS — там же и за тем же"),
    (
        "100.100.100.200",
        "IMDS Alibaba Cloud — константа протокола, одна на всех",
    ),
    (
        "192.0.0.192",
        "IMDS Azure (альтернативный) — то же самое",
    ),
    (
        "149.154.166.110",
        "дата-центр Telegram в снятом baseline FANG-31: адрес удалённой стороны, а не машины установки",
    ),
    (
        "11.0.0.1",
        "адрес сразу за 10.0.0.0/8 — пример «снаружи диапазона» в тесте",
    ),
    (
        "172.15.0.1",
        "сразу перед 172.16.0.0/12 — тот же приём в тесте границы",
    ),
    (
        "172.32.0.1",
        "сразу за 172.16.0.0/12 — тот же приём в тесте границы",
    ),
    (
        "1.2.3.4",
        "учебная заглушка в примере переменной OLLAMA_BASE_URL",
    ),
];

/// Значения `rp_name`, которые не называют ничьей установки.
const NEUTRAL_RP_NAMES: &[&str] = &["OpenFang", "OpenFang Example"];

/// Суффиксы имён, зарезервированных под примеры и локальное имя (RFC 2606/6761).
const RESERVED_HOST_SUFFIXES: &[&str] = &[
    ".test",
    ".example",
    ".invalid",
    ".localhost",
    ".example.com",
    ".example.org",
    ".example.net",
];

/// Имена хостов целиком, законные сами по себе.
const RESERVED_HOST_NAMES: &[&str] = &["localhost", "example.com", "example.org", "example.net"];

/// Каталоги, которых в обходе нет ни при каких условиях.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// Расширения файлов, которые считаются конфигурацией развёртывания.
const CONFIG_EXTENSIONS: &[&str] = &[
    "toml", "yml", "yaml", "json", "ini", "conf", "env", "service", "nix", "example",
];

// ── разбор адресов ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Addr {
    V4([u8; 4]),
    V6([u16; 8]),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FileKind {
    /// Конфигурация развёртывания: порог выше.
    Config,
    Other,
}

fn glued_left(bytes: &[u8], at: usize, colon_too: bool) -> bool {
    if at == 0 {
        return false;
    }
    let prev = bytes[at - 1];
    prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'.' || (colon_too && prev == b':')
}

fn glued_right(bytes: &[u8], at: usize) -> bool {
    bytes
        .get(at)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.')
}

/// Четвёрка IPv4, начинающаяся ровно в `at`. Возвращает `(октеты, конец)`.
fn v4_at(bytes: &[u8], at: usize) -> Option<([u8; 4], usize)> {
    if glued_left(bytes, at, false) {
        return None;
    }
    let mut octets = [0u8; 4];
    let mut i = at;
    for (k, slot) in octets.iter_mut().enumerate() {
        if k > 0 {
            if bytes.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
        let start = i;
        let mut value: u32 = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() && i - start < 3 {
            value = value * 10 + u32::from(bytes[i] - b'0');
            i += 1;
        }
        if i == start || value > 255 {
            return None;
        }
        *slot = value as u8;
    }
    if glued_right(bytes, i) {
        return None;
    }
    Some((octets, i))
}

/// Литерал IPv6, начинающийся ровно в `at`. Возвращает `(группы, конец)`.
fn v6_at(bytes: &[u8], at: usize) -> Option<([u16; 8], usize)> {
    if glued_left(bytes, at, true) {
        return None;
    }
    let mut i = at;
    while i < bytes.len() && (bytes[i].is_ascii_hexdigit() || bytes[i] == b':') {
        i += 1;
    }
    if glued_right(bytes, i) {
        return None;
    }
    let run = std::str::from_utf8(&bytes[at..i]).ok()?;
    Some((parse_v6(run)?, i))
}

/// Разбор текстовой записи IPv6. Сокращение `::` принимается от трёх групп:
/// две группы вокруг `::` — это ещё и `${x:0:2}`, и случайные байты png.
fn parse_v6(run: &str) -> Option<[u16; 8]> {
    if run.contains(":::") {
        return None;
    }
    let (head, tail, shortened) = match run.split_once("::") {
        Some((h, t)) => (h, t, true),
        None => (run, "", false),
    };
    if tail.contains("::") {
        return None;
    }
    let split = |part: &str| -> Vec<String> {
        if part.is_empty() {
            Vec::new()
        } else {
            part.split(':').map(str::to_owned).collect()
        }
    };
    let head_groups = split(head);
    let tail_groups = split(tail);
    let ok = |g: &String| !g.is_empty() && g.len() <= 4 && g.bytes().all(|c| c.is_ascii_hexdigit());
    if !head_groups.iter().all(ok) || !tail_groups.iter().all(ok) {
        return None;
    }
    let total = head_groups.len() + tail_groups.len();
    if shortened {
        if !(3..=7).contains(&total) {
            return None;
        }
    } else if total != 8 {
        return None;
    }
    let mut out = [0u16; 8];
    for (k, g) in head_groups.iter().enumerate() {
        out[k] = u16::from_str_radix(g, 16).ok()?;
    }
    for (k, g) in tail_groups.iter().enumerate() {
        out[8 - tail_groups.len() + k] = u16::from_str_radix(g, 16).ok()?;
    }
    Some(out)
}

fn parse_addr(text: &str) -> Option<Addr> {
    let bytes = text.as_bytes();
    if let Some((octets, end)) = v4_at(bytes, 0) {
        if end == bytes.len() {
            return Some(Addr::V4(octets));
        }
    }
    let (groups, end) = v6_at(bytes, 0)?;
    (end == bytes.len()).then_some(Addr::V6(groups))
}

fn v4_in(octets: [u8; 4], nets: &[([u8; 4], u32)]) -> bool {
    let value = u32::from_be_bytes(octets);
    nets.iter().any(|(net, bits)| {
        let mask = if *bits == 0 {
            0
        } else {
            u32::MAX << (32 - bits)
        };
        value & mask == u32::from_be_bytes(*net) & mask
    })
}

fn v6_in(groups: [u16; 8], nets: &[([u16; 8], u32)]) -> bool {
    let value = groups
        .iter()
        .fold(0u128, |acc, g| (acc << 16) | u128::from(*g));
    nets.iter().any(|(net, bits)| {
        let mask = if *bits == 0 {
            0
        } else {
            u128::MAX << (128 - bits)
        };
        let net = net
            .iter()
            .fold(0u128, |acc, g| (acc << 16) | u128::from(*g));
        value & mask == net & mask
    })
}

fn address_is_exempt(addr: Addr) -> bool {
    EXEMPT_ADDRESSES
        .iter()
        .any(|(text, _)| parse_addr(text) == Some(addr))
}

/// Законен ли литерал этого адреса в файле такого рода.
fn address_is_legitimate(addr: Addr, kind: FileKind) -> bool {
    if address_is_exempt(addr) {
        return true;
    }
    match (addr, kind) {
        (Addr::V4(o), FileKind::Config) => v4_in(o, V4_IN_CONFIG),
        (Addr::V4(o), FileKind::Other) => v4_in(o, V4_NOT_ROUTABLE),
        (Addr::V6(g), FileKind::Config) => v6_in(g, V6_IN_CONFIG),
        (Addr::V6(g), FileKind::Other) => v6_in(g, V6_NOT_ROUTABLE),
    }
}

/// `true`, если сразу за адресом стоит `/<цифры>` — это диапазон, а не машина.
fn is_range(bytes: &[u8], end: usize) -> bool {
    bytes.get(end) == Some(&b'/') && bytes.get(end + 1).is_some_and(u8::is_ascii_digit)
}

/// `true`, если четвёрка — хвост версии: `Chrome/131.0.0.0`. Версия приклеена
/// одним `/` к ИМЕНИ ПРОДУКТА, а не к сегменту пути, поэтому перед именем не
/// должно быть слэша: в `/srv/203.0.113.7/data` и в `https://host/1.2.3.4`
/// четвёрка остаётся адресом и разбирается как адрес.
fn is_version_tail(bytes: &[u8], at: usize) -> bool {
    if at < 2 || bytes[at - 1] != b'/' || !bytes[at - 2].is_ascii_alphanumeric() {
        return false;
    }
    let mut start = at - 1;
    while start > 0
        && matches!(bytes[start - 1], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')
    {
        start -= 1;
    }
    start == 0 || bytes[start - 1] != b'/'
}

// ── класс 1: адреса ─────────────────────────────────────────────────────────

fn addresses_in(text: &str, kind: FileKind) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    let mut line = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if bytes[i].is_ascii_digit() {
            if let Some((octets, end)) = v4_at(bytes, i) {
                if !is_range(bytes, end)
                    && !is_version_tail(bytes, i)
                    && !address_is_legitimate(Addr::V4(octets), kind)
                {
                    hits.push((line, format!("литерал адреса IPv4, {} знаков", end - i)));
                }
                i = end;
                continue;
            }
        }
        if bytes[i].is_ascii_hexdigit() || bytes[i] == b':' {
            if let Some((groups, end)) = v6_at(bytes, i) {
                if !is_range(bytes, end) && !address_is_legitimate(Addr::V6(groups), kind) {
                    hits.push((line, format!("литерал адреса IPv6, {} знаков", end - i)));
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    hits
}

// ── класс 2: значение `rp_*` ────────────────────────────────────────────────

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

/// Строковый литерал сразу за `at`: `"…"`, `'…'`, `r"…"` или `r#"…"#`.
fn string_literal_at(bytes: &[u8], at: usize) -> Option<String> {
    if at >= bytes.len() {
        return None;
    }
    if bytes[at] == b'"' || bytes[at] == b'\'' {
        let quote = bytes[at];
        // В одинарных кавычках экранирования нет (литеральная строка TOML,
        // одинарный скаляр YAML), в двойных — есть.
        let escapes = quote == b'"';
        let mut i = at + 1;
        let mut out = Vec::new();
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if escapes => {
                    if let Some(next) = bytes.get(i + 1) {
                        out.push(*next);
                    }
                    i += 2;
                }
                b'\n' => return None,
                c if c == quote => return Some(String::from_utf8_lossy(&out).into_owned()),
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
                return Some(String::from_utf8_lossy(&bytes[start..j]).into_owned());
            }
            j += 1;
        }
        return None;
    }
    None
}

/// Скаляр без кавычек сразу за `at` — только в файле конфигурации.
/// Берётся до пробела, запятой, комментария или закрывающей скобки и принимается
/// лишь тогда, когда выглядит как имя хоста: метки LDH, последняя — буквенная.
/// Так `rp_id = dash.acme.tld` разбирается, а `rp_id: cfg.auth.host` в Rust —
/// нет, потому что Rust конфигурацией не считается.
fn bare_scalar_at(bytes: &[u8], at: usize) -> Option<String> {
    let mut i = at;
    while i < bytes.len()
        && !matches!(
            bytes[i],
            b' ' | b'\t' | b'\r' | b'\n' | b',' | b'#' | b';' | b')' | b'}' | b']'
        )
    {
        i += 1;
    }
    let text = std::str::from_utf8(&bytes[at..i]).ok()?.trim_matches('"');
    if !looks_like_hostname(text) {
        return None;
    }
    Some(text.to_owned())
}

fn looks_like_hostname(text: &str) -> bool {
    let labels: Vec<&str> = text.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    if !labels.iter().all(label_ok) {
        return false;
    }
    let tld = labels[labels.len() - 1];
    tld.len() >= 2 && tld.bytes().all(|c| c.is_ascii_alphabetic())
}

/// Класс 2: значение, присвоенное `rp_id` / `rp_origin` / `rp_name`.
fn rp_values_in(text: &str, kind: FileKind) -> Vec<(usize, String)> {
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
            let mut found = None;
            let mut probe = i;
            while probe < limit {
                if let Some(literal) = string_literal_at(bytes, probe) {
                    found = Some((literal, probe));
                    break;
                }
                if kind == FileKind::Config && probe > i {
                    if let Some(scalar) = bare_scalar_at(bytes, probe) {
                        found = Some((scalar, probe));
                        break;
                    }
                }
                if is_wrapper_byte(bytes[probe]) {
                    probe += 1;
                } else {
                    break;
                }
            }
            let Some((value, offset)) = found else {
                continue;
            };
            if !rp_value_is_neutral(&value) {
                hits.push((line_of(text, offset), value));
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

/// Хост из значения: `https://x.example.test:8443/path` → `x.example.test`.
fn host_of(value: &str) -> String {
    let without_scheme = value.split_once("://").map_or(value, |(_, rest)| rest);
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.trim_matches(|c| c == '[' || c == ']');
    // Порт отрезается только у имени: у IPv6 двоеточий много.
    let host = if host.matches(':').count() == 1 {
        host.split(':').next().unwrap_or(host)
    } else {
        host
    };
    host.trim().to_ascii_lowercase()
}

fn rp_value_is_neutral(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || NEUTRAL_RP_NAMES.contains(&trimmed) {
        return true;
    }
    let host = host_of(trimmed);
    if host.is_empty() {
        return true;
    }
    if RESERVED_HOST_NAMES.contains(&host.as_str())
        || RESERVED_HOST_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix))
    {
        return true;
    }
    // Адрес в rp_* судится тем же порогом, что класс 1 вне конфигурации.
    parse_addr(&host).is_some_and(|addr| address_is_legitimate(addr, FileKind::Other))
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
/// разыменовываются: цикл ссылок повесил бы обход, — но и не пропускаются:
/// их цель разбирается как текст, см. [`text_of`].
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
            if file_type.is_dir() && !file_type.is_symlink() {
                stack.push(entry.path());
            } else {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

fn file_kind(path: &Path) -> FileKind {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.starts_with("dockerfile")
        || name.starts_with("docker-compose")
        || name.starts_with(".env")
    {
        return FileKind::Config;
    }
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if CONFIG_EXTENSIONS.contains(&extension.as_str()) {
        FileKind::Config
    } else {
        FileKind::Other
    }
}

/// Текст файла: не-UTF-8 читается как байты с заменой, потому что искомые формы
/// целиком в ASCII. Ссылка отдаёт не содержимое цели, а сам путь цели.
fn text_of(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).ok()?;
        return Some(target.to_string_lossy().into_owned());
    }
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Обход дерева. Возвращает `(сколько файлов прочитано, находки)`.
/// Находка — строка `путь:строка: форма`, без единого байта найденного значения.
fn scan_tree(root: &Path) -> (usize, Vec<String>) {
    let mut failures = Vec::new();
    let mut read = 0usize;
    for file in all_files(root) {
        let Some(text) = text_of(&file) else {
            continue;
        };
        read += 1;
        let kind = file_kind(&file);
        let shown = file.strip_prefix(root).unwrap_or(&file).display();
        for (line, what) in addresses_in(&text, kind) {
            failures.push(format!("{shown}:{line}: {what}"));
        }
        for (line, value) in rp_values_in(&text, kind) {
            failures.push(format!(
                "{shown}:{line}: rp_* — значение длиной {} символов; нужен \
                 example.test / localhost / нейтральное имя (RFC 2606). \
                 Значение не печатается: логи Actions публичны",
                value.chars().count()
            ));
        }
    }
    failures.sort();
    (read, failures)
}

// ── сами проверки ───────────────────────────────────────────────────────────

#[test]
fn no_installation_specific_values_anywhere_in_the_tree() {
    let root = workspace_root();
    let files = all_files(&root);
    assert!(
        files.len() > 200,
        "обход вернул {} файлов — дерево так не выглядит, значит обход сломан",
        files.len()
    );
    // Файлы, мимо которых страж ходил в прошлых редакциях: корень (не только
    // *.md), `.github`, `scripts`, `deploy`, `tests` — и он сам.
    for must in [
        "docker-compose.dokploy.yml",
        "openfang.toml.example",
        ".github/workflows/fork-ci.yml",
        "README.md",
        "scripts/ofrelease",
        "xtask/tests/no_installation_identifiers.rs",
    ] {
        let path = root.join(must);
        assert!(
            files.contains(&path),
            "{must} обязан быть в обходе — именно мимо таких файлов страж и ходил"
        );
    }

    let (read, failures) = scan_tree(&root);
    assert!(
        read > 200,
        "прочитано всего {read} файлов — сниффер съел дерево"
    );
    assert!(
        failures.is_empty(),
        "в публичном форке остались значения одной установки:\n  {}",
        failures.join("\n  ")
    );
}

/// Страж бесполезен, если ловит только то, что уже видел. Здесь ему
/// подсаживаются ЧУЖИЕ значения, которых в этом проекте никогда не было, — и
/// каждое обязано покраснеть. Значения собираются в рантайме: этот файл из
/// обхода не исключён, и литерал в нём покраснил бы прогон по дереву.
#[test]
fn foreign_values_are_caught_by_shape() {
    let v4 = |a: u8, b: u8, c: u8, d: u8| format!("{a}.{b}.{c}.{d}");
    let host = |labels: &[&str]| labels.join(".");

    let public = v4(198, 18, 44, 7); // 198.18/15 — benchmarking, маршрутизируемый
    let another = v4(77, 90, 43, 8);
    let v6 = ["2606", "4700", "", "1111"].join(":");
    let foreign_host = host(&["dash", "acme", "tld"]);

    // Адрес в обычном файле.
    for text in [
        format!("ALLOWED_IP={public}"),
        format!("  ssh root@{another} # deploy"),
        format!("https://[{v6}]:8443/api"),
    ] {
        assert_eq!(
            addresses_in(&text, FileKind::Other).len(),
            1,
            "чужой адрес прошёл мимо: {text:?}"
        );
    }

    // Приватный адрес: в обычном файле законен, в конфигурации — нет.
    let private = v4(10, 8, 4, 2);
    assert!(addresses_in(&private, FileKind::Other).is_empty());
    assert_eq!(addresses_in(&private, FileKind::Config).len(), 1);

    // Личность WebAuthn в формах, из которых TOML в одинарных кавычках и
    // скаляр без кавычек мимо прошлой редакции проходили.
    let key_id = ["rp", "id"].join("_");
    let key_origin = ["rp", "origin"].join("_");
    let key_name = ["rp", "name"].join("_");
    let cases: Vec<(String, FileKind)> = vec![
        (format!("{key_id} = '{foreign_host}'"), FileKind::Config),
        (format!("{key_id} = \"{foreign_host}\""), FileKind::Other),
        (format!("{key_id}: {foreign_host}"), FileKind::Config),
        (
            format!("{key_origin}: String::from(\"https://{foreign_host}\"),"),
            FileKind::Other,
        ),
        (format!("\"{key_name}\": \"Acme Agency\""), FileKind::Other),
        (
            format!("{key_origin}: r#\"https://{foreign_host}\"#,"),
            FileKind::Other,
        ),
        (
            format!("{key_name}:\n    \"Acme Agency\","),
            FileKind::Other,
        ),
        (
            format!("{key_origin}: Some(\"https://{foreign_host}\")"),
            FileKind::Other,
        ),
        (format!("{key_id}: &\"{foreign_host}\""), FileKind::Other),
    ];
    for (text, kind) in cases {
        assert_eq!(
            rp_values_in(&text, kind).len(),
            1,
            "форма прошла мимо правила rp_*: {text:?}"
        );
    }

    // Подсадка «имя-в-подмене»: суффикс example.com внутри чужого домена —
    // не резервное имя. Слабое место прошлой редакции: она сравнивала `contains`.
    let lookalike = host(&["example", "com", "acme", "tld"]);
    assert_eq!(
        rp_values_in(&format!("{key_id} = \"{lookalike}\""), FileKind::Other).len(),
        1
    );
}

/// То, что в дереве законно, обязано оставаться зелёным. Список не выдуман: это
/// формы, которые реально лежат в коде FANG-95, в тестах SSRF и в документации.
#[test]
fn legitimate_values_stay_green() {
    let legitimate_anywhere = [
        "let addr = \"127.0.0.1:4200\";",
        "OPENFANG_LISTEN=0.0.0.0:4200",
        "// Тайлнет — принимается: адрес из 100.64.0.0/10.",
        "req_from(\"100.64.0.1\")",
        "\"fd7a:115c:a1e0::/48\"",
        "\"fd7a:115c:a1e0::1\"",
        "\"2001:db8::1\"",
        "assert!(is_private_ip(&\"172.16.0.1\".parse().unwrap()));",
        "\"--user-agent=... Chrome/131.0.0.0 Safari/537.36\"",
        "http://169.254.169.254/latest/meta-data/",
        "https://example.test/api",
        "203.0.113.5:40000",
        "${OPENFANG_HOST:0:2}",
        "0.0.0.0/0",
        "8.8.8.8",
    ];
    for text in legitimate_anywhere {
        assert!(
            addresses_in(text, FileKind::Other).is_empty(),
            "ложное срабатывание на {text:?}: {:?}",
            addresses_in(text, FileKind::Other)
        );
    }

    let legitimate_in_config = [
        "OPENFANG_LISTEN: \"0.0.0.0:4200\"",
        "- \"127.0.0.1:4200:4200\"",
        "url = \"http://203.0.113.5:4200\"",
        "allow = \"10.0.0.0/8\"",
    ];
    for text in legitimate_in_config {
        assert!(
            addresses_in(text, FileKind::Config).is_empty(),
            "ложное срабатывание в конфигурации на {text:?}: {:?}",
            addresses_in(text, FileKind::Config)
        );
    }

    let key_id = ["rp", "id"].join("_");
    let key_name = ["rp", "name"].join("_");
    let key_origin = ["rp", "origin"].join("_");
    let innocent = [
        "\"agents.none_fallback\": \"Нет — добавить цепочку резервов\",".to_owned(),
        format!("{key_name}: \"OpenFang\".to_string(),"),
        format!("{key_origin}: \"https://openfang.example.test\".into(),"),
        "corp_id: \"test_corp\".to_string(),".to_owned(),
        format!("let host = config.auth.{key_id}.trim();"),
        format!("\"auth.{key_origin} must be an exact HTTPS origin\".into(),"),
        format!("pub {key_id}: String,\n    pub other: &'static str = \"anything\","),
        format!("{key_name}: format!(\"{{brand}} Ltd\"),"),
        format!("if cfg.{key_id} == \"example.test\" {{ }}"),
        format!("{key_id}: \"\","),
        format!("{key_id}: \"localhost\","),
        format!("{key_origin}: \"https://127.0.0.1:4200\","),
        format!("Defaults to auth.{key_origin} from config.toml."),
        format!("{key_id}_source: \"config\","),
    ];
    for text in innocent {
        assert!(
            rp_values_in(&text, FileKind::Other).is_empty(),
            "ложное срабатывание на {text:?}: {:?}",
            rp_values_in(&text, FileKind::Other)
        );
    }
    // В Rust скаляр без кавычек — выражение, а не значение.
    assert!(rp_values_in(&format!("{key_id}: cfg.auth.host"), FileKind::Other).is_empty());
}

/// Обход обязан доходить до всех видов файлов. Места, мимо которых прошлые
/// редакции ходили, — файл без расширения, вложенный каталог, не-UTF-8,
/// двоичный файл, символическая ссылка (значение в ЦЕЛИ, а не в содержимом) —
/// получают здесь чужое значение, и обход обязан найти каждое.
#[test]
fn the_walk_reaches_every_kind_of_file() {
    use std::io::Write as _;

    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = std::env::temp_dir().join(format!("ofguard-walk-{stamp}"));
    let deep = root.join("a").join("b").join("c");
    std::fs::create_dir_all(&deep).unwrap();

    let public = format!("{}.{}.{}.{}", 198, 18, 44, 7);
    let foreign_host = ["dash", "acme", "tld"].join(".");
    let key_id = ["rp", "id"].join("_");

    // 1. файл без расширения
    std::fs::write(root.join("Makefile-ish"), format!("HOST={public}\n")).unwrap();
    // 2. вложенный каталог
    std::fs::write(deep.join("notes.md"), format!("ssh {public}\n")).unwrap();
    // 3. не-UTF-8: латинская 1 вокруг ASCII-значения
    let mut raw = std::fs::File::create(root.join("legacy.txt")).unwrap();
    raw.write_all(&[0xE9, 0xE8, b' ']).unwrap();
    raw.write_all(public.as_bytes()).unwrap();
    raw.write_all(&[0xFF, b'\n']).unwrap();
    drop(raw);
    // 4. двоичный файл с нулевыми байтами
    let mut bin = std::fs::File::create(root.join("blob.bin")).unwrap();
    bin.write_all(&[0, 1, 2, 0]).unwrap();
    bin.write_all(public.as_bytes()).unwrap();
    bin.write_all(&[0, 0]).unwrap();
    drop(bin);
    // 5. конфигурация: приватный адрес и скаляр в одинарных кавычках
    std::fs::write(
        root.join("deploy.toml"),
        format!("listen = \"10.8.4.2:4200\"\n{key_id} = '{foreign_host}'\n"),
    )
    .unwrap();
    // 6. символическая ссылка: значение записано в ЦЕЛИ, не в содержимом
    #[cfg(unix)]
    std::os::unix::fs::symlink(format!("/srv/{public}/data"), root.join("stand-data")).unwrap();
    // 7. законный файл: краснеть не должен
    std::fs::write(root.join("clean.txt"), "127.0.0.1 and 0.0.0.0 are fine\n").unwrap();

    let (read, failures) = scan_tree(&root);
    let planted: Vec<&str> = vec![
        "Makefile-ish",
        "a/b/c/notes.md",
        "legacy.txt",
        "blob.bin",
        "deploy.toml",
        #[cfg(unix)]
        "stand-data",
    ];
    for place in &planted {
        assert!(
            failures.iter().any(|f| f.starts_with(place)),
            "подсадка в {place} прошла мимо обхода; найдено: {failures:?}"
        );
    }
    assert!(
        !failures.iter().any(|f| f.starts_with("clean.txt")),
        "законный файл покраснел: {failures:?}"
    );
    // deploy.toml краснеет дважды: приватный адрес в конфиге и значение rp_*.
    assert_eq!(
        failures
            .iter()
            .filter(|f| f.starts_with("deploy.toml"))
            .count(),
        2,
        "{failures:?}"
    );
    assert!(read >= 6, "прочитано {read} файлов из подсаженных семи");

    std::fs::remove_dir_all(&root).unwrap();
}

/// Разбор адресов — то место, где страж может стать слепым молча. Векторы
/// проверяют границы диапазонов, а не «похоже на адрес».
#[test]
fn the_address_parser_knows_its_boundaries() {
    assert_eq!(parse_addr("127.0.0.1"), Some(Addr::V4([127, 0, 0, 1])));
    assert_eq!(parse_addr("256.0.0.1"), None);
    assert_eq!(parse_addr("1.2.3"), None);
    assert_eq!(parse_addr("::1"), None); // одна группа — форма слишком слабая
    assert_eq!(
        parse_addr("2001:db8::1"),
        Some(Addr::V6([0x2001, 0x0db8, 0, 0, 0, 0, 0, 1]))
    );
    assert_eq!(parse_addr("00:32:54"), None); // это время, а не адрес
    assert_eq!(parse_addr("gggg::1"), None);

    // Хвост версии — не адрес; сегмент пути — адрес.
    assert!(addresses_in("Chrome/131.0.0.0 Safari/537.36", FileKind::Other).is_empty());
    assert_eq!(
        addresses_in(
            &format!(
                "/srv/{}/data",
                [198, 18, 44, 7].map(|o| o.to_string()).join(".")
            ),
            FileKind::Other
        )
        .len(),
        1
    );

    // Границы диапазонов: снаружи — красное, внутри — зелёное.
    assert!(!address_is_legitimate(
        Addr::V4([9, 255, 255, 255]),
        FileKind::Other
    ));
    assert!(address_is_legitimate(
        Addr::V4([10, 0, 0, 0]),
        FileKind::Other
    ));
    assert!(address_is_legitimate(
        Addr::V4([10, 255, 255, 255]),
        FileKind::Other
    ));
    assert!(!address_is_legitimate(
        Addr::V4([100, 63, 255, 255]),
        FileKind::Other
    ));
    assert!(address_is_legitimate(
        Addr::V4([100, 64, 0, 0]),
        FileKind::Other
    ));
    assert!(address_is_legitimate(
        Addr::V4([100, 127, 255, 255]),
        FileKind::Other
    ));
    assert!(!address_is_legitimate(
        Addr::V4([100, 128, 0, 0]),
        FileKind::Other
    ));
    // Тот же адрес в конфигурации — красное.
    assert!(!address_is_legitimate(
        Addr::V4([10, 0, 0, 0]),
        FileKind::Config
    ));
    assert!(address_is_legitimate(
        Addr::V4([127, 0, 0, 1]),
        FileKind::Config
    ));
}

/// Таблица исключений — единственное место, где страж пропускает
/// маршрутизируемый адрес. Пустая строка причины делает её списком «просто так».
#[test]
fn the_exemption_ledger_is_readable_and_justified() {
    for (text, why) in EXEMPT_ADDRESSES {
        assert!(
            parse_addr(text).is_some(),
            "исключение {text:?} не разбирается как адрес — оно никогда не сработает"
        );
        assert!(
            why.chars().count() >= 20,
            "исключение {text:?} без внятной причины: {why:?}"
        );
    }
    // Исключение действует, а не украшает.
    assert!(address_is_legitimate(
        Addr::V4([8, 8, 8, 8]),
        FileKind::Other
    ));
    // И не расползается на соседей.
    assert!(!address_is_legitimate(
        Addr::V4([8, 8, 8, 9]),
        FileKind::Other
    ));
}
