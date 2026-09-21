"""Go emitter: `types_gen.go` + `client_gen.go` (package aurix)."""

from __future__ import annotations

import re
from typing import List

from common import header, mixed_note, op_doc_lines, raw_variant, wrap
from ir import Api, Operation, TypeRef, pascal

INITIALISMS = {"Id": "ID", "Url": "URL", "Ip": "IP", "Ttl": "TTL", "Rtt": "RTT", "Api": "API", "Uuid": "UUID", "Json": "JSON", "Csv": "CSV", "Sso": "SSO", "Oidc": "OIDC", "Jti": "JTI", "Ssrc": "SSRC", "Ipv6": "IPv6", "Ipv4": "IPv4", "Ws": "WS", "Http": "HTTP", "Https": "HTTPS", "Udp": "UDP", "Tcp": "TCP", "Tls": "TLS", "Mos": "MOS", "Tts": "TTS", "Stt": "STT", "Sfu": "SFU", "Cpu": "CPU", "Ok": "OK", "Db": "DB"}


def go_name(name: str) -> str:
    p = pascal(name)
    parts = re.findall(r"[A-Z][a-z0-9]*|[a-z0-9]+", p)
    fixed = [INITIALISMS.get(x, x) for x in parts]
    return "".join(fixed)


def go_var(name: str) -> str:
    n = go_name(name)
    # lower the first initialism/word
    m = re.match(r"^([A-Z]+)(?=[A-Z][a-z]|$)|^([A-Z])", n)
    if m:
        head = m.group(0)
        n = head.lower() + n[len(head) :]
    if n in ("type", "func", "map", "range", "string", "default", "select", "var", "chan", "go", "if", "for", "return", "package", "import", "interface", "struct", "switch", "case", "break", "continue", "defer", "else", "fallthrough", "goto", "const"):
        n += "_"
    return n


def go_type(t: TypeRef) -> str:
    if t.kind == "string":
        return "string"
    if t.kind == "integer":
        return "int64"
    if t.kind == "number":
        return "float64"
    if t.kind == "boolean":
        return "bool"
    if t.kind == "any":
        return "any"
    if t.kind == "array":
        return "[]" + go_type(t.items)
    if t.kind == "map":
        return "map[string]" + go_type(t.items)
    if t.kind in ("ref", "enum"):
        return t.ref
    if t.kind == "union":
        return "json.RawMessage"
    raise ValueError(t.kind)


def field_type(t: TypeRef, required: bool) -> str:
    base = go_type(t)
    # Slices, maps, any and RawMessage are already nil-able.
    if t.kind in ("array", "map", "any", "union"):
        return base
    if required and not t.nullable:
        return base
    return "*" + base


def comment(lines: List[str], indent: str = "") -> str:
    return "".join(f"{indent}// {l}".rstrip() + "\n" for l in lines)


def aligned_block(entries: List, indent: str = "\t") -> List[str]:
    """Lay out `("comment", [lines])` / `("row", [cells])` entries the way gofmt does: cells of
    consecutive rows are padded into columns; comment lines start a new alignment section."""
    out: List[str] = []
    section: List[List[str]] = []

    def flush() -> None:
        if not section:
            return
        widths = [max(len(r[i]) for r in section) for i in range(len(section[0]))]
        for r in section:
            cells = [c.ljust(widths[i]) if i < len(r) - 1 else c for i, c in enumerate(r)]
            out.append(indent + " ".join(cells).rstrip())
        section.clear()

    for kind, payload in entries:
        if kind == "comment":
            flush()
            out.extend(f"{indent}// {l}".rstrip() for l in payload)
        else:
            section.append(payload)
    flush()
    return out


def query_name(op: Operation) -> str:
    return f"{pascal(op.id)}Query"


def emit_types(api: Api) -> str:
    out = [header(api, "//"), "package aurix", "", 'import "encoding/json"', "", "var _ = json.RawMessage(nil)", ""]
    for e in sorted(api.enums.values(), key=lambda e: e.name):
        out.append(comment(wrap(e.description) or [f"{e.name} enumerates the values accepted by the API."]) + f"type {e.name} string")
        out.append("")
        out.append("const (")
        out.extend(aligned_block([("row", [f"{e.name}{go_name(v)}", e.name, f'= "{v}"']) for v in e.values]))
        out.append(")")
        out.append("")
    for m in sorted(api.models.values(), key=lambda m: m.name):
        out.append(comment(wrap(m.description) or [f"{m.name} is the `{m.name}` schema."]) + f"type {m.name} struct {{")
        entries = []
        for f in m.fields:
            omit = "" if (f.required and not f.type.nullable) else ",omitempty"
            if f.required and f.type.nullable:
                omit = ""
            if doc := wrap(f.description):
                entries.append(("comment", doc))
            entries.append(("row", [go_name(f.name), field_type(f.type, f.required), f'`json:"{f.name}{omit}"`']))
        out.extend(aligned_block(entries))
        out.append("}")
        out.append("")
    for op in api.operations:
        if op.query_params:
            out.append(f"// {query_name(op)} holds the query parameters of `{op.id}`.")
            out.append(f"type {query_name(op)} struct {{")
            entries = []
            for p in op.query_params:
                if doc := wrap(p.description):
                    entries.append(("comment", doc))
                entries.append(("row", [go_name(p.name), field_type(p.type, False)]))
            out.extend(aligned_block(entries))
            out.append("}")
            out.append("")
    return "\n".join(out).rstrip() + "\n"


def emit_client(api: Api) -> str:
    out = [header(api, "//"), "package aurix", "", "import (", '\t"context"', '\t"encoding/json"', '\t"net/url"', '\t"strconv"', ")", "", "var (", "\t_ = json.RawMessage(nil)", "\t_ = strconv.Itoa", ")", ""]
    for op in api.operations:
        variant = raw_variant(op)
        if variant in ("typed", "both"):
            out.append(method(op, typed=True))
        if variant in ("raw", "both"):
            out.append(method(op, typed=False))
    return "\n".join(out).rstrip() + "\n"


def query_encoder(op: Operation) -> str:
    if not op.query_params:
        return "\tvar q url.Values\n"
    lines = ["\tq := url.Values{}", "\tif query != nil {"]
    for p in op.query_params:
        f = f"query.{go_name(p.name)}"
        t = p.type
        lines.append(f"\t\tif {f} != nil {{")
        if t.kind == "string":
            lines.append(f'\t\t\tq.Set("{p.name}", *{f})')
        elif t.kind == "enum":
            lines.append(f'\t\t\tq.Set("{p.name}", string(*{f}))')
        elif t.kind == "integer":
            lines.append(f'\t\t\tq.Set("{p.name}", strconv.FormatInt(*{f}, 10))')
        elif t.kind == "number":
            lines.append(f'\t\t\tq.Set("{p.name}", strconv.FormatFloat(*{f}, \'g\', -1, 64))')
        elif t.kind == "boolean":
            lines.append(f'\t\t\tq.Set("{p.name}", strconv.FormatBool(*{f}))')
        else:
            raise ValueError(f"query param {p.name} of {op.id} has unsupported type {t.kind}")
        lines.append("\t\t}")
    lines.append("\t}")
    return "\n".join(lines) + "\n"


def method(op: Operation, typed: bool) -> str:
    name = go_name(op.id) + ("" if typed else "Raw")
    params = ["ctx context.Context"]
    for p in op.path_params:
        params.append(f"{go_var(p.name)} {go_type(p.type)}")
    if op.body is not None:
        params.append(f"body {'' if op.body_required else '*'}{go_type(op.body)}")
    if op.query_params:
        params.append(f"query *{query_name(op)}")
    params.append("opts ...RequestOption")
    lines = op_doc_lines(op)
    lines[0] = f"{name} — {lines[0]}" if lines else name
    if typed and (note := mixed_note(op)):
        lines += ["", note.replace("`Raw` variant", f"`{name}Raw`")]
    if not typed:
        lines += ["", f"Returns the raw body ({', '.join(op.response_media) or 'empty'})."]
    path = op.path
    for p in op.path_params:
        path = path.replace("{" + p.name + "}", '"+url.PathEscape(' + go_var(p.name) + ')+"')
    path_expr = f'"{path}"'
    path_expr = path_expr.replace('+""', "").replace('""+', "")
    body_arg = "body" if op.body is not None else "nil"
    pre = ""
    if op.body is not None and not op.body_required:
        pre = "\tvar bodyArg any\n\tif body != nil {\n\t\tbodyArg = body\n\t}\n"
        body_arg = "bodyArg"
    if typed:
        if op.response is None:
            ret = "error"
            call = f'\treturn c.doJSON(ctx, "{op.method}", {path_expr}, q, {body_arg}, nil, opts)\n'
        else:
            rt = go_type(op.response)
            if op.response.kind in ("array", "map", "union", "any"):
                ret = f"({rt}, error)"
                call = f'\tvar out {rt}\n\terr := c.doJSON(ctx, "{op.method}", {path_expr}, q, {body_arg}, &out, opts)\n\treturn out, err\n'
            else:
                ret = f"(*{rt}, error)"
                call = f'\tvar out {rt}\n\tif err := c.doJSON(ctx, "{op.method}", {path_expr}, q, {body_arg}, &out, opts); err != nil {{\n\t\treturn nil, err\n\t}}\n\treturn &out, nil\n'
    else:
        ret = "(*RawResponse, error)"
        call = f'\treturn c.doRaw(ctx, "{op.method}", {path_expr}, q, {body_arg}, opts, "*/*")\n'
    return comment(lines) + f"func (c *Client) {name}({', '.join(params)}) {ret} {{\n" + query_encoder(op) + pre + call + "}\n"
