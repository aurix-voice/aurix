"""``GET /v1/events`` server-sent events as a blocking iterator with reconnection and
``Last-Event-ID`` resume."""

from __future__ import annotations

import json
import socket
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any, BinaryIO, Iterable, Iterator, List, Optional, Sequence, Union

from ._http import BaseClient, RequestOptions, retry_after_seconds
from .errors import AurixError, AurixNetworkError


@dataclass
class SseEvent:
    type: str
    id: Optional[str]
    data: Any
    raw: str


@dataclass
class _Raw:
    type: Optional[str] = None
    id: Optional[str] = None
    data: str = ""
    retry: Optional[int] = None


def parse_sse(lines: Iterable[bytes]) -> Iterator[_Raw]:
    """Minimal SSE parser over an iterable of raw lines (as yielded by ``HTTPResponse``)."""
    cur = _Raw()
    data: List[str] = []
    for raw_line in lines:
        line = raw_line.decode("utf-8", errors="replace").rstrip("\r\n")
        if line == "":
            if data or cur.type is not None or cur.id is not None:
                cur.data = "\n".join(data)
                yield cur
            cur, data = _Raw(), []
            continue
        if line.startswith(":"):
            continue
        field, sep, value = line.partition(":")
        if sep and value.startswith(" "):
            value = value[1:]
        if field == "event":
            cur.type = value
        elif field == "data":
            data.append(value)
        elif field == "id" and "\0" not in value:
            cur.id = value
        elif field == "retry" and value.isdigit():
            cur.retry = int(value)


def event_stream(
    client: BaseClient,
    *,
    types: Optional[Union[str, Sequence[str]]] = None,
    reconnect: bool = True,
    reconnect_delay: float = 1.0,
    max_reconnect_delay: float = 30.0,
    last_event_id: Optional[str] = None,
    options: Optional[RequestOptions] = None,
    should_stop: Optional[Any] = None,
) -> Iterator[SseEvent]:
    """Iterate the application's event stream. Stop by breaking out of the loop or returning
    ``True`` from ``should_stop()``. On ``lagged`` fetch ``/v1/events/snapshot`` to resync."""
    query = {"types": ",".join(types) if isinstance(types, (list, tuple)) else types} if types else None
    url = client.url("/v1/events", query)
    delay = reconnect_delay
    while True:
        if should_stop and should_stop():
            return
        headers = client._headers(options, False, "text/event-stream")
        if last_event_id:
            headers["Last-Event-ID"] = last_event_id
        try:
            req = urllib.request.Request(url, method="GET", headers=headers)
            resp: BinaryIO = client._opener.open(req, timeout=options.timeout if options and options.timeout else None)
        except urllib.error.HTTPError as e:
            content = e.read()
            resp_headers = {k: v for k, v in e.headers.items()}
            err = AurixError.from_response(e.code, e.headers.get("content-type", ""), content, resp_headers, "GET", "/v1/events")
            if reconnect and (e.code == 429 or e.code >= 500):
                ra = retry_after_seconds(resp_headers)
                time.sleep(delay if ra is None else ra)
                delay = min(max_reconnect_delay, delay * 2)
                continue
            raise err from e
        except (urllib.error.URLError, socket.timeout, OSError) as e:
            if not reconnect:
                raise AurixNetworkError(str(e), "GET", "/v1/events", e) from e
            time.sleep(delay)
            delay = min(max_reconnect_delay, delay * 2)
            continue
        delay = reconnect_delay
        try:
            with resp:
                for ev in parse_sse(resp):
                    if ev.id is not None:
                        last_event_id = ev.id
                    if ev.retry is not None:
                        delay = ev.retry / 1000.0
                    try:
                        data: Any = json.loads(ev.data)
                    except ValueError:
                        data = ev.data
                    yield SseEvent(ev.type or "message", ev.id, data, ev.data)
                    if should_stop and should_stop():
                        return
        except (socket.timeout, OSError) as e:
            if not reconnect:
                raise AurixNetworkError(str(e), "GET", "/v1/events", e) from e
        if not reconnect:
            return
        time.sleep(delay)
        delay = min(max_reconnect_delay, delay * 2)
