# OpenFang — Agent Instructions

## ЭТА МАШИНА — прочитать до всего остального

Ветки `ours` больше нет (удалена 23 августа как дубликат `main`); ствол — `main`.

Всё, что ниже «Project Overview», унаследовано от апстрима и написано под Windows.
Намерение тех разделов верное — юнит-тесты проходят и на мёртвом коде, поэтому живая
проверка обязательна — но механика на этой машине не исполняется, проверено:

- **rust на хосте нет** (`which cargo` пуст, каталога `target/` не существует), поэтому
  `cargo build/test/clippy` из «Build & Verify Workflow» здесь не запускаются;
- `tasklist`/`taskkill` не существуют; бинарь называется `openfang`, не `openfang.exe`
  (`crates/openfang-cli/Cargo.toml`, `[[bin]] name = "openfang"`);
- в рабочей области 13 крейтов в `crates/` плюс `xtask` — 14 членов workspace.

Как здесь на самом деле собирают и проверяют (оба инструмента идут в контейнере и оба
выходят с кодом 3, когда им мало места на диске — **код 3 это отказ инструмента, а не
дефект патча**; порог у `ofmutate` плоский 12 ГБ, у `ofgate` — арифметика от измеренных
22 ГБ полной цели сборки):

```bash
sh /root/.claude/skills/fang-upgrade/scripts/ofgate <worktree> [--only fmt,clippy,test] [--wait]
python3 /root/.claude/skills/fang-upgrade/scripts/ofmutate <worktree> --test <ф> -p <крейт>
```

**Гейт — Fork CI на пул-реквесте, а не `ofgate`.** Ветка → PR → зелёный CI (две джобы:
`fmt + clippy + test` и сборка образа) → слияние; прямо в `main` не пушить.
`ofgate` гоняет ТРИ команды из джобы `check`, дословно и в порядке CI:
`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace -- --test-threads=2`. Чего у него нет, а у CI есть (сверено по
`.github/workflows/fork-ci.yml` 2026-08-24): шаг «версии тулчейна сходятся во всех
четырёх местах», установка тулчейна и rust-cache, системные библиотеки Tauri и вторая
джоба со сборкой Dockerfile. Зелёный `ofgate` значит «три команды cargo прошли на моём
дереве», и ничего шире. Его блок вердикта идёт в отчёт дословно — закрывающая строка
несёт sha256 блока, строки шагов — размеры и sha256 логов; подлинность блока
доказывается перезапуском гейта, а не чтением отчёта.

**`ofcheck-rs` не может покраснеть.** Его последняя строка —
`cargo check $ARGS 2>&1 | tail -40`, а код конвейера это код `tail`. Проверено:
`docker run --rm alpine sh -c "false | tail -40"; echo $?` → `0`. Ни один его вызов не
вернёт ненулевой код, поэтому доказательством годности патча он не служит; как быстрый
`cargo check` одного крейта — служит, но смотреть надо на вывод, а не на код выхода.

**Потолок `timeout` у Bash — 600000 мс.** Запрос больше молча обрезается до десяти
минут, и вызов вернёт `Command timed out after 10m 0s`, хотя команда жива. Долгое
запускать `run_in_background: true`. Полный `ofgate` на холодном томе измерен дважды —
1248 с и 1615 с (fmt 3 · clippy 433 и 435 · test 812 и 1175) — и на переднем плане
стартовать отказывается: выходит с кодом 2 и печатает готовую команду с `--wait`.

**`ofmutate` — обязательный шаг для любого патча с тестом.** Он откатывает продакшн-ханки
патча, оставляя тестовые, и требует, чтобы тест покраснел. Вердикт `ТАВТОЛОГИЯ` или
`passed=0` означает, что теста фактически нет: за пять спринтов это случилось 10 раз.

**Протокол работы, приёмку и шаблон задания сабагенту** держит
[`docs/subagent-task-template.md`](docs/subagent-task-template.md) — блоки оттуда
копируются в задание дословно; пересказ по памяти был тем механизмом, которым пункты
терялись от спринта к спринту. Форма отчёта — не абзац, а
[`docs/subagent-report.schema.json`](docs/subagent-report.schema.json): отчёт без полей
`gate`, `mutate`, `red_before_green`, `unverified`, `claims_without_proof` отклоняется
механически (`python3 docs/subagent-report.schema.check.py` показывает, на чём именно).
Три правила оттуда действуют и в одиночной работе:

1. **Красное до зелёного**, и записью считается вывод прогона, а не утверждение о нём.
2. **Нет прогона — нет фразы.** Докстринг и комментарий — такое же утверждение, как
   ответ; патч, чинящий ложное утверждение, добавляет не прозу, а удаление, измеренное
   число или прогон по каждой названной поверхности.
3. **Три круга с одним классом дефекта — решение о подходе, а не четвёртый круг.**

Прод — контейнер `openfang-openfang-1` на `127.0.0.1:4200`, **не трогать**. Стенд —
`openfang-staging` на `127.0.0.1:4201`. На 2026-08-23 у них **один и тот же image ID**
(`sha256:49d9ba8b274a…`), оба тегированы `ghcr.io/kyzdes/fang-upgrade:1009ed230dcb…`
(из GHCR, не локальная сборка) — стенд изолирует порт и данные, но не код. Проверять
`docker inspect --format '{{.Image}}'`, а не имя тега.

## Project Overview
OpenFang is an open-source Agent Operating System written in Rust (14 crates).
- Config: `~/.openfang/config.toml`
- Default API: `http://127.0.0.1:4200`
- CLI binary: `target/release/openfang.exe` (or `target/debug/openfang.exe`)

## Build & Verify Workflow
After every feature implementation, run ALL THREE checks:
```bash
cargo build --workspace --lib          # Must compile (use --lib if exe is locked)
cargo test --workspace                 # All tests must pass (currently 1744+)
cargo clippy --workspace --all-targets -- -D warnings  # Zero warnings
```

## MANDATORY: Live Integration Testing
**After implementing any new endpoint, feature, or wiring change, you MUST run live integration tests.** Unit tests alone are not enough — they can pass while the feature is actually dead code. Live tests catch:
- Missing route registrations in server.rs
- Config fields not being deserialized from TOML
- Type mismatches between kernel and API layers
- Endpoints that compile but return wrong/empty data

### How to Run Live Integration Tests

#### Step 1: Stop any running daemon
```bash
tasklist | grep -i openfang
taskkill //PID <pid> //F
# Wait 2-3 seconds for port to release
sleep 3
```

#### Step 2: Build fresh release binary
```bash
cargo build --release -p openfang-cli
```

#### Step 3: Start daemon with required API keys
```bash
GROQ_API_KEY=<key> target/release/openfang.exe start &
sleep 6  # Wait for full boot
curl -s http://127.0.0.1:4200/api/health  # Verify it's up
```
The daemon command is `start` (not `daemon`).

#### Step 4: Test every new endpoint
```bash
# GET endpoints — verify they return real data, not empty/null
curl -s http://127.0.0.1:4200/api/<new-endpoint>

# POST/PUT endpoints — send real payloads
curl -s -X POST http://127.0.0.1:4200/api/<endpoint> \
  -H "Content-Type: application/json" \
  -d '{"field": "value"}'

# Verify write endpoints persist — read back after writing
curl -s -X PUT http://127.0.0.1:4200/api/<endpoint> -d '...'
curl -s http://127.0.0.1:4200/api/<endpoint>  # Should reflect the update
```

#### Step 5: Test real LLM integration
```bash
# Get an agent ID
curl -s http://127.0.0.1:4200/api/agents | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['id'])"

# Send a real message (triggers actual LLM call to Groq/OpenAI)
curl -s -X POST "http://127.0.0.1:4200/api/agents/<id>/message" \
  -H "Content-Type: application/json" \
  -d '{"message": "Say hello in 5 words."}'
```

#### Step 6: Verify side effects
After an LLM call, verify that any metering/cost/usage tracking updated:
```bash
curl -s http://127.0.0.1:4200/api/budget       # Cost should have increased
curl -s http://127.0.0.1:4200/api/budget/agents  # Per-agent spend should show
```

#### Step 7: Verify dashboard HTML
```bash
# Check that new UI components exist in the served HTML
curl -s http://127.0.0.1:4200/ | grep -c "newComponentName"
# Should return > 0
```

#### Step 8: Cleanup
```bash
tasklist | grep -i openfang
taskkill //PID <pid> //F
```

### Key API Endpoints for Testing
| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/api/health` | GET | Basic health check |
| `/api/agents` | GET | List all agents |
| `/api/agents/{id}/message` | POST | Send message (triggers LLM) |
| `/api/budget` | GET/PUT | Global budget status/update |
| `/api/budget/agents` | GET | Per-agent cost ranking |
| `/api/budget/agents/{id}` | GET | Single agent budget detail |
| `/api/network/status` | GET | OFP network status |
| `/api/peers` | GET | Connected OFP peers |
| `/api/a2a/agents` | GET | External A2A agents |
| `/api/a2a/discover` | POST | Discover A2A agent at URL |
| `/api/a2a/send` | POST | Send task to external A2A agent |
| `/api/a2a/tasks/{id}/status` | GET | Check external A2A task status |

## Architecture Notes
- **Don't touch `openfang-cli`** — user is actively building the interactive CLI
- `KernelHandle` trait avoids circular deps between runtime and kernel
- `AppState` in `server.rs` bridges kernel to API routes
- New routes must be registered in `server.rs` router AND implemented in `routes.rs`
- Dashboard is Alpine.js SPA in `static/index_body.html` — new tabs need both HTML and JS data/methods
- Config fields need: struct field + `#[serde(default)]` + Default impl entry + Serialize/Deserialize derives

## Common Gotchas
- `openfang.exe` may be locked if daemon is running — use `--lib` flag or kill daemon first
- `PeerRegistry` is `Option<PeerRegistry>` on kernel but `Option<Arc<PeerRegistry>>` on `AppState` — wrap with `.as_ref().map(|r| Arc::new(r.clone()))`
- Config fields added to `KernelConfig` struct MUST also be added to the `Default` impl or build fails
- `AgentLoopResult` field is `.response` not `.response_text`
- CLI command to start daemon is `start` not `daemon`
- On Windows: use `taskkill //PID <pid> //F` (double slashes in MSYS2/Git Bash)
