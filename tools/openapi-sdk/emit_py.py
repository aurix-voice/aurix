"""Python emitter: `aurix_server/generated/types.py` + `aurix_server/generated/client.py`."""

from __future__ import annotations

import keyword
from typing import List

from common import header, mixed_note, op_doc_lines, raw_variant, wrap
from ir import Api, Operation, TypeRef, pascal, snake


def ident(name: str) -> str:
    n = snake(name)
    return n + "_" if keyword.iskeyword(n) else n


def py_type(t: TypeRef, prefix: str = "") -> str:
    if t.kind == "string":
        base = "str"
    elif t.kind == "integer":
        base = "int"
    elif t.kind == "number":
        base = "float"
    elif t.kind == "boolean":
        base = "bool"
    elif t.kind == "any":
        base = "Any"
    elif t.kind == "array":
        base = f"List[{py_type(t.items, prefix)}]"
    elif t.kind == "map":
        base = f"Dict[str, {py_type(t.items, prefix)}]"
    elif t.kind in ("ref", "enum"):
        base = f'"{prefix}{t.ref}"'
    elif t.kind == "union":
        base = "Union[" + ", ".join(py_type(v, prefix) for v in t.variants) + "]"
    else:
        raise ValueError(t.kind)
    return f"Optional[{base}]" if t.nullable else base


def query_name(op: Operation) -> str:
    return f"{pascal(op.id)}Query"


def docstring(lines: List[str], indent: str) -> str:
    if not lines:
        return ""
    if len(lines) == 1:
        return f'{indent}"""{lines[0]}"""\n'
    body = "".join((f"{indent}{l}" if l else "") + "\n" for l in lines)
    return f'{indent}"""{lines[0]}\n\n' + "".join((f"{indent}{l}" if l else "") + "\n" for l in lines[1:]) + f'{indent}"""\n'


def emit_types(api: Api) -> str:
    out = [header(api, "#"), "# ruff: noqa: E501", "from __future__ import annotations", "", "import sys", "from typing import Any, Dict, List, Literal, Optional, TypedDict, Union", "", "if sys.version_info >= (3, 11):", "    from typing import NotRequired", "else:", "    from typing_extensions import NotRequired", "", ""]
    for e in sorted(api.enums.values(), key=lambda e: e.name):
        out.append(f"{e.name} = Literal[" + ", ".join(f'"{v}"' for v in e.values) + "]")
        if e.description:
            out.append(f'"""{" ".join(wrap(e.description))}"""')
        out.append(f"{e.name}_VALUES: List[{e.name}] = [" + ", ".join(f'"{v}"' for v in e.values) + "]")
        out.append("")
    out.append("")
    for m in sorted(api.models.values(), key=lambda m: m.name):
        out.append(f'{m.name} = TypedDict(')
        out.append(f'    "{m.name}",')
        out.append("    {")
        for f in m.fields:
            t = py_type(f.type)
            if not f.required:
                t = f"NotRequired[{t}]"
            comment = f"  # {' '.join(wrap(f.description, 200))}" if f.description else ""
            out.append(f'        "{f.name}": {t},{comment}')
        out.append("    },")
        out.append(")")
        if m.description:
            out.append(f'"""{" ".join(wrap(m.description, 400))}"""')
        out.append("")
    out.append("")
    for op in api.operations:
        if op.query_params:
            out.append(f"{query_name(op)} = TypedDict(")
            out.append(f'    "{query_name(op)}",')
            out.append("    {")
            for p in op.query_params:
                t = py_type(p.type)
                if not p.required:
                    t = f"NotRequired[{t}]"
                out.append(f'        "{p.name}": {t},')
            out.append("    },")
            out.append(")")
            out.append(f'"""Query parameters of `{op.id}`."""')
            out.append("")
    return "\n".join(out).rstrip() + "\n"


def emit_client(api: Api) -> str:
    out = [header(api, "#"), "# ruff: noqa: E501", "from __future__ import annotations", "", "from typing import Any, Dict, List, Optional, Union", "", "from .. import types as T", "from .._http import BaseClient, RawResponse, RequestOptions", "from .._http import path_segment as _p", "", ""]
    out.append("class AurixClient(BaseClient):")
    out.append('    """Typed synchronous REST client for the Aurix control plane.\n\n    Every method maps to one OpenAPI operation; payload types live in `aurix_server.types`.\n    """')
    out.append("")
    for op in api.operations:
        variant = raw_variant(op)
        if variant in ("typed", "both"):
            out.append(method(op, typed=True, is_async=False))
        if variant in ("raw", "both"):
            out.append(method(op, typed=False, is_async=False))
    out.append("")
    out.append("class AsyncAurixClient:")
    out.append('    """`asyncio` façade over `AurixClient`: every call runs the blocking request in a worker thread."""')
    out.append("")
    out.append("    def __init__(self, client: AurixClient):")
    out.append("        self.sync = client")
    out.append("")
    out.append("    async def _run(self, fn: Any, *args: Any, **kwargs: Any) -> Any:")
    out.append("        import asyncio")
    out.append("        import functools")
    out.append("")
    out.append("        loop = asyncio.get_running_loop()")
    out.append("        return await loop.run_in_executor(None, functools.partial(fn, *args, **kwargs))")
    out.append("")
    for op in api.operations:
        variant = raw_variant(op)
        if variant in ("typed", "both"):
            out.append(method(op, typed=True, is_async=True))
        if variant in ("raw", "both"):
            out.append(method(op, typed=False, is_async=True))
    return "\n".join(out).rstrip() + "\n"


def method(op: Operation, typed: bool, is_async: bool) -> str:
    name = ident(op.id) + ("" if typed else "_raw")
    params = ["self"]
    call_args: List[str] = []
    for p in op.path_params:
        params.append(f"{ident(p.name)}: {py_type(p.type, 'T.')}")
        call_args.append(ident(p.name))
    if op.body is not None:
        if op.body_required:
            params.append(f"body: {py_type(op.body, 'T.')}")
        else:
            params.append(f"body: Optional[{py_type(op.body, 'T.')}] = None")
        call_args.append("body")
    kw: List[str] = []
    if op.query_params:
        for p in op.query_params:
            t = py_type(p.type, "T.")
            if p.required:
                kw.append(f"{ident(p.name)}: {t}")
            else:
                kw.append(f"{ident(p.name)}: Optional[{t}] = None")
    kw.append("options: Optional[RequestOptions] = None")
    params += ["*"] + kw
    lines = op_doc_lines(op)
    if typed and (note := mixed_note(op)):
        lines += ["", note.replace("`Raw` variant", f"`{name}_raw`")]
    if not typed:
        lines += ["", f"Returns the raw body ({', '.join(op.response_media) or 'empty'})."]
    if typed:
        ret = "None" if op.response is None else py_type(op.response, "T.")
    else:
        ret = "RawResponse"
    sig = f"    {'async ' if is_async else ''}def {name}({', '.join(params)}) -> {ret}:\n"
    if is_async:
        fwd = call_args + [f"{ident(p.name)}={ident(p.name)}" for p in op.query_params] + ["options=options"]
        body = f"        return await self._run(self.sync.{name}, {', '.join(fwd)})  # type: ignore[no-any-return]\n"
        return sig + docstring(lines, "        ") + body
    path = op.path
    for p in op.path_params:
        path = path.replace("{" + p.name + "}", "{_p(" + ident(p.name) + ")}")
    path_expr = f'f"{path}"' if op.path_params else f'"{path}"'
    query = ""
    if op.query_params:
        query = ", query={" + ", ".join(f'"{p.name}": {ident(p.name)}' for p in op.query_params) + "}"
    body_arg = ", body=body" if op.body is not None else ""
    fn = "_json" if typed else "_raw"
    if typed and ret == "None":
        body = f'        self._json("{op.method}", {path_expr}{body_arg}{query}, options=options)'
    else:
        body = f'        return self.{fn}("{op.method}", {path_expr}{body_arg}{query}, options=options)'
        if typed:
            body += "  # type: ignore[no-any-return]"
    return sig + docstring(lines, "        ") + body + "\n"
