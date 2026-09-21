"""TypeScript (Node) emitter: `src/generated/types.ts` + `src/generated/client.ts`."""

from __future__ import annotations

from typing import Dict, List

from common import header, mixed_note, op_doc_lines, raw_variant, wrap
from ir import Api, Operation, Param, TypeRef, camel, pascal

RESERVED = {"default", "delete", "function", "new", "class", "import", "export", "var", "let", "const", "from", "to", "in"}


def ident(name: str) -> str:
    n = camel(name)
    return n + "_" if n in RESERVED else n


def ts_type(t: TypeRef, types_prefix: str = "") -> str:
    if t.kind == "string":
        base = "string"
    elif t.kind in ("integer", "number"):
        base = "number"
    elif t.kind == "boolean":
        base = "boolean"
    elif t.kind == "any":
        base = "unknown"
    elif t.kind == "array":
        inner = ts_type(t.items, types_prefix)
        base = f"Array<{inner}>" if ("|" in inner or " " in inner) else f"{inner}[]"
    elif t.kind == "map":
        base = f"Record<string, {ts_type(t.items, types_prefix)}>"
    elif t.kind in ("ref", "enum"):
        base = types_prefix + t.ref
    elif t.kind == "union":
        base = " | ".join(ts_type(v, types_prefix) for v in t.variants)
    else:
        raise ValueError(t.kind)
    return f"{base} | null" if t.nullable else base


def doc(lines: List[str], indent: str = "") -> str:
    if not lines:
        return ""
    if len(lines) == 1:
        return f"{indent}/** {lines[0]} */\n"
    body = "".join(f"{indent} * {l}".rstrip() + "\n" for l in lines)
    return f"{indent}/**\n{body}{indent} */\n"


def query_name(op: Operation) -> str:
    return f"{pascal(op.id)}Query"


def emit_types(api: Api) -> str:
    out = [header(api, "//"), "/* eslint-disable */", ""]
    for e in sorted(api.enums.values(), key=lambda e: e.name):
        out.append(doc(wrap(e.description)) + f"export type {e.name} = " + " | ".join(f'"{v}"' for v in e.values) + ";")
        out.append(
            f"export const {e.name}Values: readonly {e.name}[] = [" + ", ".join(f'"{v}"' for v in e.values) + "] as const;"
        )
        out.append("")
    for m in sorted(api.models.values(), key=lambda m: m.name):
        out.append(doc(wrap(m.description)) + f"export interface {m.name} {{")
        for f in m.fields:
            out.append(doc(wrap(f.description), "  ") + f"  {quote(f.name)}{'' if f.required else '?'}: {ts_type(f.type)};")
        if m.additional is not None:
            out.append(f"  [key: string]: {ts_type(m.additional)} | undefined;" if not m.fields else "  [key: string]: unknown;")
        out.append("}")
        out.append("")
    for op in api.operations:
        if op.query_params:
            out.append(f"/** Query parameters of `{op.id}`. */")
            out.append(f"export interface {query_name(op)} {{")
            for p in op.query_params:
                out.append(doc(wrap(p.description), "  ") + f"  {quote(p.name)}{'' if p.required else '?'}: {ts_type(p.type)};")
            out.append("}")
            out.append("")
    return "\n".join(out).rstrip() + "\n"


def quote(name: str) -> str:
    return name if name.isidentifier() else f'"{name}"'


def path_template(op: Operation) -> str:
    path = op.path
    for p in op.path_params:
        path = path.replace("{" + p.name + "}", "${encodeURIComponent(" + ident(p.name) + ")}")
    return "`" + path + "`" if op.path_params else f'"{path}"'


def emit_client(api: Api) -> str:
    out = [header(api, "//"), "/* eslint-disable */"]
    out.append('import { AurixHttp, type RawResponse, type RequestOptions } from "../http.js";')
    out.append('import type * as T from "./types.js";')
    out.append("")
    out.append(doc(["Typed REST client for the Aurix control plane.", "", "Every method maps to one OpenAPI operation; see `T` for payload types."]) + "export class AurixClient extends AurixHttp {")
    for op in api.operations:
        variant = raw_variant(op)
        if variant in ("typed", "both"):
            out.append(method(op, typed=True))
        if variant in ("raw", "both"):
            out.append(method(op, typed=False))
    out.append("}")
    return "\n".join(out).rstrip() + "\n"


def method(op: Operation, typed: bool) -> str:
    name = ident(op.id) + ("" if typed else "Raw")
    params: List[str] = []
    for p in op.path_params:
        params.append(f"{ident(p.name)}: {ts_type(p.type, 'T.')}")
    if op.body is not None:
        params.append(f"body{'' if op.body_required else '?'}: {ts_type(op.body, 'T.')}")
    if op.query_params:
        required = any(p.required for p in op.query_params)
        params.append(f"query{'' if required else '?'}: T.{query_name(op)}")
    params.append("options?: RequestOptions")
    lines = op_doc_lines(op)
    if typed and (note := mixed_note(op)):
        lines += ["", note]
    if not typed:
        lines += ["", f"Returns the raw body ({', '.join(op.response_media) or 'empty'})."]
    if typed:
        ret = "void" if op.response is None else ts_type(op.response, "T.")
        call = f'this.json<{ret}>("{op.method}", {path_template(op)}, {{'
    else:
        ret = "RawResponse"
        call = f'this.raw("{op.method}", {path_template(op)}, {{'
    args = []
    if op.body is not None:
        args.append("body")
    if op.query_params:
        args.append("query")
    args.append("...options")
    body = f"    return {call} {', '.join(args)} }});"
    return doc(lines, "  ") + f"  {name}({', '.join(params)}): Promise<{ret}> {{\n{body}\n  }}\n"
