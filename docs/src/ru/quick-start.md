# Быстрый старт: два игрока говорят за пять минут

Понадобятся: Rust (1.88+), Docker, Node 20+ (для браузерного демо) и зависимости сборки
`pkg-config`, `libssl-dev`, `cmake` (имена пакетов Debian; libopus собирается из встроенных
исходников). Всё ниже крутится на одной машине, на loopback, в режиме разработки —
[продакшен-путь](../operations/deployment.md) добавляет TLS, публичный IP, TURN и настоящие секреты.

## 1. Поднять PostgreSQL, Redis и ноду (≈ 2 мин, в основном компиляция)

```bash
docker run -d --name aurix-pg -e POSTGRES_USER=aurix -e POSTGRES_PASSWORD=aurix -e POSTGRES_DB=aurix -p 127.0.0.1:5432:5432 postgres:16-alpine
docker run -d --name aurix-redis -p 127.0.0.1:6379:6379 redis:7-alpine

cargo build --locked --bin aurix-server --bin aurix   # нода и CLI
aurix=target/debug/aurix

mkdir -p ~/.config/aurix && (umask 077; openssl rand -hex 24 > ~/.config/aurix/bootstrap)   # для /admin/setup
AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN=$(cat ~/.config/aurix/bootstrap) target/debug/aurix-server &
$aurix ready                                           # ready после миграций
```

Нода слушает `:8080` (REST), `:8081` (WebSocket), `:10000/udp` (медиа), `:3478` (TURN) и
`:4040` (метрики). `GET /health` отвечает сразу; `GET /ready` дополнительно проверяет PostgreSQL
и Redis. Миграции встроены в бинарник и применяются на старте.

## 2. Первый администратор и первое приложение (30 с)

`POST /admin/setup` открыт, пока нет ни одного администратора; дальше его открывает только
bootstrap-токен выше (CLI всегда его отправляет). CLI никогда не печатает секреты: токен
оператора сохраняется в профиль, а API-ключ приложения — сервер показывает его ровно один раз —
сразу уходит в файл с правами `0600`.

```bash
$aurix config init --name dev --server http://localhost:8080 \
    --api-key-file ~/.config/aurix/mygame.api-key --set-default   # файл появится через минуту
$aurix admin setup --email root@example.com --display-name Root \
    --bootstrap-token-file ~/.config/aurix/bootstrap             # пароль — со stdin
$aurix admin login --email root@example.com --save                # JWT оператора → профиль, 0600
$aurix app create --name MyGame --key-out ~/.config/aurix/mygame.api-key
```

## 3. Канал и два игровых токена — это делает ваш игровой бэкенд (30 с)

```bash
CH=$($aurix channel create --name lobby --type positional --field id)
ALICE=$($aurix token issue --external-id steam:1 --display-name Alice --channel "$CH" --field token)
BOB=$($aurix token issue --external-id steam:2 --display-name Bob   --channel "$CH" --field token)
```

API-ключ остаётся на машине, где выполнялись эти команды. Игроки получают только короткоживущий
JWT (`$ALICE`, `$BOB`) — грант внутри него говорит, в какие каналы можно войти и можно ли
говорить и слушать (`--channel ID:flags`, по умолчанию `jsr`). Эта граница — вся модель
безопасности голосового деплоя; подробно — в [Client flow](../getting-started/client-flow.md) и
[Tenancy, credentials and permissions](../concepts/auth.md).

## 4. Услышать их (1 мин)

Браузер, движок не нужен:

```bash
cd sdk/web && npm ci && npm run demo          # http://localhost:5173, две вкладки
```

Вставьте `$ALICE` в одну вкладку и `$BOB` в другую, введите id канала, *Connect*, *Join*,
говорите. Демо-страница показывает ростер, индикаторы речи, чат, выбор устройств, усиление
микрофона и канал для проверки микрофона (echo); позиционное затухание задаётся вызовом
`updatePosition()` из кода игры, а не страницей. Headless-вариант из корня репозитория против
той же ноды:

```bash
AURIX_E2E_API_KEY=$(cat ~/.config/aurix/mygame.api-key) \
  cargo test -p aurix-client --test e2e_live -- --test-threads=1
```

Дальше — SDK под ваш движок: [Web](../sdk/web.md), [Unity](../sdk/unity.md) (импортируйте
сэмпл *Voice quick start* и нажмите Play), [Unreal / native C ABI](../sdk/native.md),
[Godot](../sdk/godot.md). Каждому из них нужны только JWT и URL WebSocket из шага 3 — и ничего
больше.

## 5. Посмотреть, что произошло

```bash
$aurix channel participants "$CH"        # ростер с флагами speaking / muted
$aurix user session <session_id>         # живые MOS, RTT, потери, джиттер одного игрока
$aurix events tail --count 10            # participant.joined / left / quality.alert … (SSE)
curl -s localhost:4040/metrics | grep aurix_active_sessions
```

## Без CLI: тот же поток в `curl`

Всё, что делает CLI, — вызовы из `api/openapi.json`; `curl`-эквиваленты пригодятся, когда те же
шаги встраиваются в ваш бэкенд.

```bash
curl -X POST localhost:8080/admin/setup -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>","display_name":"Root"}'
ADMIN=$(curl -s -X POST localhost:8080/admin/login -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>"}' | jq -r .token)

# создать приложение; в ответе — первый API-ключ приложения (показывается один раз)
curl -X POST localhost:8080/v1/apps -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' -d '{"name":"MyGame"}'
```

Всё дальнейшее делается API-ключом (`X-API-Key: aurx_…` или `Authorization: Bearer aurx_…`):

```bash
# канал
curl -X POST localhost:8080/v1/channels -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"name":"lobby","config":{"channel_type":"positional","max_participants":64}}'

# токен игрока (короткоживущий JWT с каналами и правами)
curl -X POST localhost:8080/v1/tokens -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"external_id":"steam:7656119","display_name":"Alice",
       "channels":[{"channel_id":"<channel uuid>","speak":true,"receive":true}]}'

# одноразовый action-токен (login | join | kick | mute | unmute), одно использование, 90 с
curl -X POST localhost:8080/v1/tokens/action -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"action":"join","external_id":"steam:7656119","channel_id":"<channel uuid>","speak":true}'
# kick/mute/unmute дополнительно требуют moderation:write, цель и действующего пользователя:
#   {"action":"kick","user_id":"<moderator uuid>","channel_id":"<channel>","target_user_id":"<player>"}

# ad-hoc канал: POST /v1/channels не нужен — грант называет канал, и он создаётся при первом
# входе (и удаляется, когда выходит последний участник). Id выводится из имени, поэтому ответ
# сразу сообщает channel_id, а все токены с тем же именем попадают в один канал.
# Работает и в /v1/tokens, и в /v1/tokens/action (join).
curl -X POST localhost:8080/v1/tokens -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"external_id":"steam:7656119","display_name":"Alice",
       "channels":[{"ad_hoc":{"name":"match-8f3a","channel_type":"team","max_participants":10}}]}'

# модерация всего канала: все присутствующие, кроме перечисленных
curl -X POST localhost:8080/v1/moderation/mute-all -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"channel_id":"<channel uuid>","muted":true,"except":["<game master uuid>"]}'
curl -X POST localhost:8080/v1/moderation/kick-all -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"channel_id":"<channel uuid>","reason":"round over"}'
```

## Дальше

* Что происходит на проводе: [Client flow](../getting-started/client-flow.md).
* Переходите с Vivox, Agora или Photon Voice: [Миграция](migration.md).
* Продакшен: [Deployment and configuration](../operations/deployment.md) — одна нода через
  `docker compose`, затем [High availability](../operations/high-availability.md) и
  [Scaling out](../operations/scaling.md).
* Готовые token-серверы на Node, Python, Go и C#:
  [Server SDKs and token servers](../backend/server-sdks.md).
