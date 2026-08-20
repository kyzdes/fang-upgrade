//! Заставить пересборку, когда меняется то, что попадает в `/api/version`.
//!
//! `option_env!` читается на этапе компиляции, но cargo по умолчанию **не считает
//! переменную окружения входом** — сменив GIT_SHA, получишь тот же объектный файл
//! из кэша и залипшее старое значение. С кэш-монтированием `target/` в Dockerfile
//! это гарантированно: без этих директив каждая следующая сборка сообщала бы SHA
//! первой.
//!
//! Ровно тот класс ошибки, который форк вычищает пять спринтов: сборка говорит не
//! то, чем является. Здесь она была бы вдвойне обидной — поле существует именно
//! затем, чтобы отличать сборки.
fn main() {
    for var in ["GIT_SHA", "GIT_DESCRIBE", "BUILD_DATE"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    // rustc сам себя не объявляет — спрашиваем и прокидываем как обычную переменную.
    let rustc = std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !rustc.is_empty() {
        println!("cargo:rustc-env=RUSTC_VERSION={rustc}");
    }
}
