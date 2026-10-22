# aurix-server-sdk

Server-side (game backend) SDK for the [Aurix](https://github.com/aurix-voice/aurix) voice platform
REST API: token issuance, channels, users, moderation, analytics, webhooks and the SSE event stream.
Sync (`AurixClient`) and async (`AsyncAurixClient`) clients, typed dicts for every request and
response, no third-party dependencies (Python 3.9+). Generated from `api/openapi.json`.

```bash
pip install aurix-server-sdk
```

## Issue a voice token

```python
import os
from aurix_server import AurixClient

aurix = AurixClient(os.environ["AURIX_URL"], api_key=os.environ["AURIX_API_KEY"])

res = aurix.issue_token({
    "external_id": player.id,            # your stable account id
    "display_name": player.name,
    "channels": [{"ad_hoc": {"name": f"match-{match_id}", "channel_type": "team"},
                  "join": True, "speak": True, "receive": True}],
    "region": "eu_west",                 # optional hint
})
return {"token": res["token"], "endpoint": res["endpoint"]}   # never the API key
```

`AsyncAurixClient` has the same methods as coroutines. Options: `timeout` (15 s), `max_retries` (2),
`max_backoff` (5 s), extra `headers`.

## Webhooks and events

```python
from aurix_server import parse_webhook, event_stream

event = parse_webhook(os.environ["AURIX_WEBHOOK_SECRET"], headers, raw_body)   # HMAC + timestamp check

for ev in event_stream(aurix, types=["participant.joined", "participant.left"]):
    print(ev.type, ev.data)
```

## Credentials and errors

The client takes one credential: an **API key** of the application (`tokens:issue`, `channels:*`,
`moderation:*`… scopes as configured on the key), or an admin / bootstrap token for console-grade
calls. Keep it on your backend — players only ever receive the short-lived voice token.

Every method maps 1:1 to the REST API (`api/openapi.json`); `4xx/5xx` become a typed error with
`status`, `code` and the request id, and idempotent calls are retried on network errors,
`429` (honouring `Retry-After`) and `502/503/504`.

## Docs

- Server SDKs and token servers: https://github.com/aurix-voice/aurix/blob/main/docs/src/backend/server-sdks.md
- Token-server examples (Node / Python / Go / C#): https://github.com/aurix-voice/aurix/tree/main/sdk/server/examples/token-server
- REST API reference: https://github.com/aurix-voice/aurix/blob/main/api/openapi.json
- Changelog: https://github.com/aurix-voice/aurix/blob/main/CHANGELOG.md

Apache-2.0 — part of the open-source [Aurix voice platform](https://github.com/aurix-voice/aurix).
