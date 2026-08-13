# Развёртывание форка OpenFang на чистом сервере

> Публичная копия. Читается напрямую, без логина:
> `curl -sL https://raw.githubusercontent.com/kyzdes/openfang-patched/main/INSTALL-AGENT.md`
>
> Форк: <https://github.com/kyzdes/openfang-patched> · апстрим: <https://github.com/RightNow-AI/openfang>

Задание для агента, у которого есть root на свежей машине. Ставит наш патченый форк,
проверяет его и подключает скилл `openfang`.

Читается сверху вниз. Каждый шаг заканчивается проверкой — если она не сошлась, дальше
не идти, а разобраться: половина шагов ниже написана потому, что кто-то уже прошёл мимо
такой проверки.

**Правило для всего документа:** команда, вернувшая ноль, не означает, что работа сделана.
Проверять надо результат, а не код возврата. Это не общая мудрость — конкретно здесь
`docker build` успешно собирает образ с пустым бинарём, а `cargo clippy | grep` возвращает
код от `grep`.

---

## 0. Что понадобится

- Linux с root, 4+ ядра, 8 ГБ RAM, **40 ГБ свободного диска** (сборка Rust съедает 10–15 ГБ,
  и это без учёта повторов)
- Docker с BuildKit (в комплекте с любым современным docker)
- ключ хотя бы одного LLM-провайдера, совместимого с OpenAI API

Проверить до начала, потому что нехватка места проявляется не сообщением «нет места», а
падением `cargo` посреди сборки:

```bash
df -h / | awk 'NR==2{print "свободно: "$4}'
nproc; free -g | awk 'NR==2{print "RAM: "$2"G"}'
docker version --format '{{.Server.Version}}'
```

---

## 1. Клонировать форк

```bash
mkdir -p /opt && cd /opt
git clone https://github.com/kyzdes/openfang-patched.git openfang
cd /opt/openfang
```

Прочитать `FORK-NOTES.md` — там что починено и что осталось сломанным. Раздел
«Known issues, not fixed here» важнее списка правок: он объясняет поведение, которое иначе
примешь за свою ошибку.

**Проверка:**
```bash
grep -c 'effective_fallback_base_url' crates/openfang-kernel/src/kernel.rs   # ожидается 8
grep -c 'redact_reqwest_error' crates/openfang-channels/src/telegram.rs      # ожидается 16
```
Нули означают, что склонирован апстрим, а не форк.

---

## 2. Закрыть две дыры в compose ДО первого запуска

Это делается заранее, а не после, потому что демон при первом старте создаёт конфигурацию
и начинает слушать порт.

В форке уже лежит `docker-compose.override.yml` с обеими правками — убедиться, что он на
месте, и понять почему:

```bash
cat docker-compose.override.yml
```

**Порт.** Апстрим публикует `4200:4200`, то есть на `0.0.0.0`. **Docker обходит UFW:**
публикация порта пишет правило DNAT в PREROUTING, а трафик уходит в цепочку FORWARD, минуя
INPUT — поэтому `ufw deny 4200` не защищает. Единственный надёжный способ — не публиковать
наружу:

```yaml
ports: !override
  - "127.0.0.1:4200:4200"
  # плюс, если нужен доступ извне, адрес приватного интерфейса:
  # - "100.x.y.z:4200:4200"   # tailscale/wireguard
```

`OPENFANG_LISTEN` внутри контейнера обязан быть `0.0.0.0:4200`, иначе через опубликованный
порт до него не достучаться: по умолчанию демон слушает `127.0.0.1:50051`, что внутри
контейнера означает «только сам контейнер».

**Пустые ключи провайдеров.** Апстримовый compose объявляет `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY` и другие как пустые-но-присутствующие. OpenFang читает «присутствует» как
«настроен»: на пустой строке он поднимается, заявляя провайдера anthropic, и включает
embedding-драйвер OpenAI — то есть **текст начинает уходить наружу**. Поэтому в override
стоит `environment: !override`, который заменяет блок целиком, а не дополняет его.

Настоящие ключи класть в `.env` рядом, он подхватывается `env_file`:

```bash
umask 077 && cat > /opt/openfang/.env <<'EOF'
MYPROVIDER_API_KEY=<ключ>
EOF
```

---

## 3. Собрать

Первая сборка ~12 минут на 4 ядрах. Быстрее, ценой чуть более медленного бинаря:

```bash
docker compose build --build-arg LTO=false --build-arg CODEGEN_UNITS=16
```

**Проверка, без которой сборке нельзя верить.** В `Dockerfile` стоят BuildKit cache mounts,
а кэш-маунт не виден следующей стадии через `COPY --from`. Если правило скопировать бинарь
внутри того же `RUN` нарушить, сборка пройдёт успешно и даст образ с пустым бинарём. Молча.

```bash
docker run --rm --entrypoint sh openfang-openfang:latest -c \
  'stat -c "%s байт" /usr/local/bin/openfang && openfang --version'
```
Ожидается порядка 83 МБ и строка версии. Несколько килобайт — сборка сломана.

---

## 4. Запустить и задать ключ

```bash
cd /opt/openfang && docker compose up -d
until curl -sf -m 3 http://127.0.0.1:4200/api/health; do sleep 2; done; echo
```

Демон создаст `config.toml` в томе. **Задать `api_key` обязательно** — без него API открыт
всему, что дотянется до порта, а агенты умеют выполнять shell-команды.

```bash
V=/var/lib/docker/volumes/openfang_openfang-data/_data/config.toml
KEY="of-$(python3 -c 'import secrets;print(secrets.token_hex(24))')"
python3 - "$KEY" <<'PY'
import sys, pathlib, re
key = sys.argv[1]
p = pathlib.Path("/var/lib/docker/volumes/openfang_openfang-data/_data/config.toml")
t = p.read_text()
# api_key должен быть НА ВЕРХНЕМ УРОВНЕ, то есть до первой [секции] —
# иначе он станет полем этой секции и молча не будет действовать
if re.search(r'(?m)^api_key\s*=', t):
    t = re.sub(r'(?m)^api_key\s*=.*$', 'api_key = "%s"' % key, t, count=1)
else:
    t = 'api_key = "%s"\n\n' % key + t
p.write_text(t)
print("ключ записан")
PY
echo "$KEY"     # сохранить, он понадобится для дашборда
docker restart openfang-openfang-1
```

**Проверка авторизации — и тут легко обмануться.** `GET /api/agents` **публичен по замыслу**
(его читает дашборд), поэтому 200 без ключа — это норма, а не дыра. Проверять надо на записи:

```bash
IP=127.0.0.1
curl -s -o /dev/null -w 'без ключа: %{http_code} (ждём 401)\n' -X POST \
     -H 'Content-Type: application/json' -d '{}' http://$IP:4200/api/agents
curl -s -o /dev/null -w 'с ключом:  %{http_code} (ждём 400 — тело пустое, но авторизация прошла)\n' \
     -X POST -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
     -d '{}' http://$IP:4200/api/agents
```

---

## 5. Провайдер и модель

```bash
V=/var/lib/docker/volumes/openfang_openfang-data/_data/config.toml
cat >> $V <<'EOF'

[provider_urls]
myprovider = "https://api.example.com/v1"
EOF
```

`[default_model]` уже создан демоном — поправить провайдера, модель, `api_key_env` и
`base_url` под себя.

**Тонкость, стоившая нам отдельного патча:** блок `[[fallback_models]]` в манифесте агента
раньше наследовал `base_url` от `[default_model]`, то есть ключ одного провайдера уходил на
хост другого. В форке это исправлено — фолбэк резолвит адрес из `[provider_urls]` по имени
своего провайдера. Но привычка указывать `base_url` в фолбэке явно всё равно полезна.

После правки конфига **перезапустить контейнер**, не полагаясь на горячую перезагрузку:
часть действий она честно помечает `deferred` (в форке; в апстриме молча врала, что применила).

```bash
docker restart openfang-openfang-1
until curl -sf -m 3 http://127.0.0.1:4200/api/health; do sleep 2; done; echo
```

---

## 6. Проверить живьём

Список агентов, создание, сообщение:

```bash
KEY=$(python3 -c "
import re
for ln in open('/var/lib/docker/volumes/openfang_openfang-data/_data/config.toml'):
    if ln.startswith('['): break
    m = re.match(r'\s*api_key\s*=\s*\"([^\"]+)\"', ln)
    if m: print(m.group(1)); break")
AID=$(curl -s http://127.0.0.1:4200/api/agents | python3 -c "
import json,sys; d=json.load(sys.stdin); a=d if isinstance(d,list) else d.get('agents',d)
print(a[0]['id'] if a else '')")
curl -s -m 600 -X POST -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"message":"Ответь одним словом: OK"}' \
  http://127.0.0.1:4200/api/agents/$AID/message | python3 -m json.tool
```

В ответе форка есть поля, которых нет в апстриме: `model_used`, `provider_used` и `calls[]`.
**Если ответ пришёл от резервной модели, это будет видно** — в апстриме подмена происходит
молча и портит данные незаметно.

Таймаут `-m 600` не перестраховка: `/message` реально идёт минуты, а на тяжёлых задачах до
получаса. Клиент, оборвавшийся по таймауту, выглядит как пустой ответ демона, хотя агент
продолжает работать.

---

## 7. Скилл `openfang`

Скилл — это набор инструкций и утилит (`ofctl`, `ofdoctor`, `ofhand`, `ofcron`, `ofbackup`),
который избавляет от ручной возни с curl и Bearer-заголовками.

```bash
mkdir -p ~/.claude/skills
git clone https://github.com/kyzdes/openfang-skill-private.git ~/.claude/skills/openfang
chmod +x ~/.claude/skills/openfang/scripts/*
export PATH="$HOME/.claude/skills/openfang/scripts:$PATH"
echo 'export PATH="$HOME/.claude/skills/openfang/scripts:$PATH"' >> ~/.bashrc
```

**Репозиторий скилла приватный.** Нужен доступ к аккаунту владельца — `gh auth login` или
деплой-ключ. Если доступа нет, этот шаг пропускается целиком: всё остальное в документе
делается голым `curl`, скилл лишь избавляет от ручных Bearer-заголовков. Дальнейшие команды
приведены в обоих вариантах.

**Проверка (со скиллом):**
```bash
ofctl --show-key-source     # должен показать путь к config.toml и длину ключа, но не сам ключ
ofctl -x version GET /api/health
ofdoctor                    # health-проход, среди прочего ищет секреты в публичных ответах
```

**То же без скилла:**
```bash
curl -s http://127.0.0.1:4200/api/health
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:4200/api/agents | python3 -m json.tool | head
```

Если путь к тому отличается от нашего, задать явно:
```bash
export OPENFANG_CONFIG=/var/lib/docker/volumes/openfang_openfang-data/_data/config.toml
export OPENFANG_URL=http://127.0.0.1:4200
```

**Ловушка `ofctl`:** таймаут по умолчанию 30 секунд. Для `/message` всегда `-t 600`, иначе
получишь пустой ответ ровно через 30 секунд при работающем агенте.

---

## 8. Что сделать до того, как считать сервер готовым

**Бэкап тома.** Он маленький (десятки МБ) и содержит всё: агентов, конфиг, сессии, ключи.

```bash
docker run --rm -v openfang_openfang-data:/d -v /root:/b alpine \
  tar czf /b/openfang-backup-$(date +%F).tar.gz -C /d .
```

**Проверить, что наружу ничего не торчит.** Не с самой машины — запрос на собственный
публичный IP уходит через loopback и минует firewall, показывая ложную картину. Проверять
с другой машины:

```bash
nmap -Pn -p 4200 <публичный-ip-сервера>     # ожидается closed/filtered
```

**Записать, что именно развёрнуто:**
```bash
cd /opt/openfang && git log --oneline -1
docker inspect openfang-openfang-1 --format '{{.Image}}'
```

---

## Что пойдёт не так — и что это значит

| Симптом | Причина |
|---|---|
| `Missing Authorization: Bearer <api_key>` при верном ключе | ключ извлечён неверно. `grep '^api_key'` цепляет и `api_key_env`, давая склейку. Брать только верхнеуровневый, до первой `[секции]` |
| HTTP 400 с пустым телом | почти всегда битый ключ или тело не JSON |
| `GET /api/agents` отдаёт 200 без ключа | так и задумано, это публичное чтение. Проверять авторизацию на POST |
| агент «забывает» после ~20 сообщений | штатное окно сессии, `POST /api/agents/{id}/session/reset` |
| `Max iterations exceeded` → HTTP 500 | лимит итераций мал. Правило: свод по N файлам требует `max_iterations >= N + 3`. Поменять без пересоздания агента нельзя |
| ход упал по лимиту — расход не записался | известный дефект, учёт теряется целиком |
| документ вышел вдвое тоньше ожидаемого | две причины: подмена модели (в форке видна в ответе) и `file_read`, который режет файл до доли контекстного окна и об этом не сообщает |
| `hand` исчез после `docker restart` | известная ловушка v0.6.9 |
| канал Telegram в цикле `409 Conflict` | ту же очередь читает кто-то ещё. Telegram допускает одного читателя; искать второй экземпляр с тем же токеном |

---

## Чего делать не надо

**Не вызывать `getUpdates` руками при активном канале Telegram.** Очередь допускает одного
читателя: ручной вызов отбирает её у демона, и тот уходит в экспоненциальный откат. Мы так
сломали приём сообщений ровно в процессе его починки.

**Не пробовать запись на живых сущностях.** Проверять, принимает ли API запись ключа
провайдера, надо на несуществующем имени провайдера — иначе затрёшь рабочий ключ.

**Не верить отчёту агента о выполненной работе.** «Готово, записал файл» и наличие файла —
разные утверждения. Проверять артефакт: существует, не пуст, осмысленного размера.

**Не публиковать конфиг и логи не глядя.** В `config.toml` первой строкой лежит `api_key`;
в логах апстрима — токен бота Telegram (в форке починено). Перед тем как приложить файл
куда-либо, прогнать по нему поиск секретов — и **сначала проверить сам поиск на заведомом
секрете**: `grep -E` с шаблоном `\{24,\}` вместо `{24,}` молча не находит ничего и говорит
«чисто».
