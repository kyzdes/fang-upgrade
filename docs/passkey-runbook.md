# OpenFang passkey runbook

Операционный порядок для установки, где вход в дашборд закрыт пасскеем:
включение, выдача приглашения, осмотр слотов, отзыв, замена.

Этот рунбук не содержит секретов и не должен их получить. Ссылка-приглашение и
машинный API-ключ не коммитятся, не попадают в логи и не передаются аргументом
командной строки.

Всюду ниже `openfang.example.test` — плейсхолдер: подставьте домен своей
установки. `alice` — плейсхолдер слага слота.

## Личность установки

- Дашборд: `https://openfang.example.test`
- RP ID: `openfang.example.test`
- RP origin: `https://openfang.example.test`
- Контейнер: узнать во время работы —
  `docker ps --filter name=openfang --format '{{.Names}}'`
- Постоянный дом внутри контейнера: `/data`

Конфигурация:

```toml
[auth]
enabled = true
rp_id = "openfang.example.test"
rp_origin = "https://openfang.example.test"
rp_name = "OpenFang"
session_ttl_hours = 168
```

`rp_name` — то имя, которое показывает аутентификатор при регистрации и которое
стоит над заголовком страниц `/login` и `/register`. Оно берётся из конфига, а
не из исходника. Оставленное на умолчании (`OpenFang` — то же слово, что и
словесный знак в шапке карточки) оно не рисуется вовсе: имя не печатается
дважды.

OpenFang отказывается поднимать пасскей-аутентификацию при неполной
конфигурации, не-HTTPS origin, origin с портом или путём, а также если хост
origin отличается от RP ID.

## Перед миграцией или выкаткой

1. Снять WAL-безопасную резервную копию (`ofbackup` из установленного скилла
   `fang-upgrade`).
2. Проверить копию до того, как трогать контейнер или схему SQLite.
3. Записать текущий образ/коммит и убедиться, что `/api/health` отвечает.

Миграция добавляет пасскей-пользователей, учётные данные, хеши приглашений и
хеши серверных сессий. Данные агентов и сессий она не меняет.

## Выдать приглашение

Слот заводится по требованию — жёсткого списка людей в коде нет. Ссылка несёт
токен во **фрагменте** (`/register#<токен>`), поэтому не уходит на сервер и не
оседает в логах обратного прокси; в базе лежит только SHA-256 от неё. Показать
ссылку второй раз нельзя — восстанавливать нечего.

```bash
container_name="$(docker ps --filter name=openfang --format '{{.Names}}' | head -n1)"
docker exec "$container_name" openfang auth invite alice \
  --name 'Alice' \
  --expires-hours 72 \
  --output /tmp/openfang-passkey-invite.txt
docker cp "$container_name":/tmp/openfang-passkey-invite.txt ./OPENFANG-PASSKEY-INVITE.txt
chmod 0600 ./OPENFANG-PASSKEY-INVITE.txt
docker exec "$container_name" unlink /tmp/openfang-passkey-invite.txt
```

`--output` создаёт файл режимом `0600` и **отказывается перезаписывать
существующий**. Без `--output` ссылка печатается в терминал — это осознанный
выбор между буфером терминала и файлом, а не забывчивость.

Перенесите локальный файл в согласованное место передачи секретов. Не выводите
его `cat`, не загружайте на GitHub и не пересылайте содержимое в чат.

## Осмотреть слоты

```bash
container_name="$(docker ps --filter name=openfang --format '{{.Names}}' | head -n1)"
docker exec "$container_name" openfang auth list --json
```

Показываются только слаг, отображаемое имя, число активных учётных данных и срок
истечения приглашения. Ни учётных данных, ни токена сессии, ни ссылки-приглашения
эта команда не печатает.

## Отозвать слот

Отзыв немедленно обесценивает пасскей слота, все его серверные сессии и любое
ожидающее приглашение:

```bash
docker exec "$container_name" openfang auth revoke alice
```

## Сбросить слот и выдать замену

Файл вывода не должен существовать заранее:

```bash
docker exec "$container_name" openfang auth reset-slot alice \
  --expires-hours 72 \
  --output /tmp/alice-passkey-replacement.txt
docker cp "$container_name":/tmp/alice-passkey-replacement.txt ./ALICE-PASSKEY-REPLACEMENT.txt
chmod 0600 ./ALICE-PASSKEY-REPLACEMENT.txt
docker exec "$container_name" unlink /tmp/alice-passkey-replacement.txt
```

Для нового временного слота под проверку добавьте `--display-name`. После
проверки удалите его целиком: `openfang auth revoke smoke --delete`. Флаг
`--delete` предназначен для одноразовых тестовых личностей; обычные слоты
остаются как отозванная история.

## Что проверить после включения

Список проверок, а не обещаний: прогоните их на своей установке.

- `GET /api/health` публичен и отвечает `200`.
- Анонимный `GET /` уводит на `/login`.
- Анонимные защищённые JSON-маршруты, включая `/api/sessions`, отвечают JSON `401`.
- Старый парольный `POST /api/auth/login` отвечает `404`.
- Вход по пасскею ставит `__Host-openfang_session` с `Secure`, `HttpOnly`,
  `SameSite=Strict`, `Path=/` и без `Domain`.
- Повторённая или истёкшая церемония не проходит.
- Браузерные мутации и браузерные WebSocket требуют точного `Origin` = RP origin.
- Машинный `Authorization: Bearer …` продолжает работать на защищённых HTTP- и
  WebSocket-маршрутах, но дашборд этот ключ не спрашивает и не хранит.
- Перезапуск контейнера сохраняет зарегистрированные пасскеи и неистёкшие сессии
  в именованном томе.
