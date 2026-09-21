"""C# emitter: `Generated/Types.cs` + `Generated/Client.cs` (namespace Aurix.Server)."""

from __future__ import annotations

from typing import List

from common import header, mixed_note, op_doc_lines, raw_variant, wrap
from ir import Api, Operation, TypeRef, camel, pascal

CS_KEYWORDS = {"event", "object", "string", "params", "base", "default", "from", "to", "in", "out", "ref", "class", "new", "operator", "lock", "checked", "fixed"}


def prop_name(name: str, owner: str) -> str:
    p = pascal(name)
    if p == owner:
        p += "Value"  # a member cannot have the same name as its enclosing type
    return p


def var_name(name: str) -> str:
    n = camel(name)
    return "@" + n if n in CS_KEYWORDS else n


def cs_type(t: TypeRef) -> str:
    if t.kind == "string":
        base = "string"
    elif t.kind == "integer":
        base = "long"
    elif t.kind == "number":
        base = "double"
    elif t.kind == "boolean":
        base = "bool"
    elif t.kind == "any":
        base = "object"
    elif t.kind == "array":
        base = f"List<{cs_type(t.items)}>"
    elif t.kind == "map":
        base = f"Dictionary<string, {cs_type(t.items)}>"
    elif t.kind == "enum":
        base = "string"  # string constants (see the static class of the same name)
    elif t.kind == "ref":
        base = t.ref
    elif t.kind == "union":
        base = "JsonElement"
    else:
        raise ValueError(t.kind)
    return f"{base}?" if (t.nullable or t.kind == "any") else base


def is_value_type(t: TypeRef) -> bool:
    return t.kind in ("integer", "number", "boolean", "union")


def xml_doc(lines: List[str], indent: str) -> str:
    if not lines:
        return ""
    esc = [l.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;") for l in lines]
    out = [f"{indent}/// <summary>"]
    for l in esc:
        out.append(f"{indent}/// {l}".rstrip())
    out.append(f"{indent}/// </summary>")
    return "\n".join(out) + "\n"


def query_name(op: Operation) -> str:
    return f"{pascal(op.id)}Query"


def emit_types(api: Api) -> str:
    out = [header(api, "//"), "#nullable enable", "using System.Collections.Generic;", "using System.Text.Json;", "using System.Text.Json.Serialization;", "", "namespace Aurix.Server;", ""]
    for e in sorted(api.enums.values(), key=lambda e: e.name):
        out.append(xml_doc(wrap(e.description) or [f"Known values of the `{e.name}` enum (fields typed as string stay forward compatible)."], "") + f"public static class {e.name}")
        out.append("{")
        for v in e.values:
            out.append(f'    public const string {pascal(v)} = "{v}";')
        out.append(f"    public static readonly IReadOnlyList<string> All = new[] {{ " + ", ".join(pascal(v) for v in e.values) + " };")
        out.append("}")
        out.append("")
    for m in sorted(api.models.values(), key=lambda m: m.name):
        out.append(xml_doc(wrap(m.description), "") + f"public sealed record {m.name}")
        out.append("{")
        for f in m.fields:
            t = cs_type(f.type)
            required = f.required
            if not required and not t.endswith("?"):
                t += "?"
            attrs = [f'[JsonPropertyName("{f.name}")]']
            if not required:
                attrs.append("[JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]")
            mod = "required " if required else ""
            out.append(xml_doc(wrap(f.description), "    ") + "    " + " ".join(attrs))
            out.append(f"    public {mod}{t} {prop_name(f.name, m.name)} {{ get; init; }}")
            out.append("")
        if out[-1] == "":
            out.pop()
        out.append("}")
        out.append("")
    for op in api.operations:
        if op.query_params:
            out.append(f"/// <summary>Query parameters of <c>{op.id}</c>.</summary>")
            out.append(f"public sealed record {query_name(op)}")
            out.append("{")
            for p in op.query_params:
                t = cs_type(p.type)
                if not t.endswith("?"):
                    t += "?"
                out.append(xml_doc(wrap(p.description), "    ") + f"    public {t} {prop_name(p.name, query_name(op))} {{ get; init; }}")
            out.append("}")
            out.append("")
    return "\n".join(out).rstrip() + "\n"


def emit_client(api: Api) -> str:
    out = [header(api, "//"), "#nullable enable", "using System;", "using System.Collections.Generic;", "using System.Globalization;", "using System.Net.Http;", "using System.Text.Json;", "using System.Threading;", "using System.Threading.Tasks;", "", "namespace Aurix.Server;", ""]
    out.append(xml_doc(["Typed REST client for the Aurix control plane.", "Every method maps to one OpenAPI operation.", ], "") + "public sealed partial class AurixClient : AurixHttp")
    out.append("{")
    out.append("    public AurixClient(AurixClientOptions options) : base(options) { }")
    out.append("")
    for op in api.operations:
        variant = raw_variant(op)
        if variant in ("typed", "both"):
            out.append(method(op, typed=True))
        if variant in ("raw", "both"):
            out.append(method(op, typed=False))
    out.append("}")
    return "\n".join(out).rstrip() + "\n"


def query_encoder(op: Operation) -> str:
    if not op.query_params:
        return "        Dictionary<string, string>? q = null;\n"
    lines = ["        var q = new Dictionary<string, string>();", "        if (query is not null)", "        {"]
    for p in op.query_params:
        f = f"query.{prop_name(p.name, query_name(op))}"
        t = p.type
        if t.kind in ("string", "enum"):
            lines.append(f'            if ({f} is not null) q["{p.name}"] = {f};')
        elif t.kind == "integer":
            lines.append(f'            if ({f} is not null) q["{p.name}"] = {f}.Value.ToString(CultureInfo.InvariantCulture);')
        elif t.kind == "number":
            lines.append(f'            if ({f} is not null) q["{p.name}"] = {f}.Value.ToString("R", CultureInfo.InvariantCulture);')
        elif t.kind == "boolean":
            lines.append(f'            if ({f} is not null) q["{p.name}"] = {f}.Value ? "true" : "false";')
        else:
            raise ValueError(f"query param {p.name} of {op.id} has unsupported type {t.kind}")
    lines.append("        }")
    return "\n".join(lines) + "\n"


def method(op: Operation, typed: bool) -> str:
    name = pascal(op.id) + ("Raw" if not typed else "") + "Async"
    params: List[str] = []
    for p in op.path_params:
        params.append(f"{cs_type(p.type)} {var_name(p.name)}")
    if op.body is not None:
        bt = cs_type(op.body)
        params.append(f"{bt} body" if op.body_required else f"{bt}? body = null")
    if op.query_params:
        params.append(f"{query_name(op)}? query = null")
    params.append("RequestOptions? options = null")
    params.append("CancellationToken cancellationToken = default")
    lines = op_doc_lines(op)
    if typed and (note := mixed_note(op)):
        lines += ["", note.replace("`Raw` variant", f"`{pascal(op.id)}RawAsync`")]
    if not typed:
        lines += ["", f"Returns the raw body ({', '.join(op.response_media) or 'empty'})."]
    path = op.path
    for p in op.path_params:
        path = path.replace("{" + p.name + "}", '" + Uri.EscapeDataString(' + var_name(p.name) + ') + "')
    path_expr = f'"{path}"'
    path_expr = path_expr.replace(' + ""', "").replace('"" + ', "")
    body_arg = "body" if op.body is not None else "null"
    if typed:
        if op.response is None:
            ret = "Task"
            call = f'        return SendJsonAsync({path_expr}, HttpMethod.{op.method.capitalize()}, q, {body_arg}, options, cancellationToken);'
        else:
            rt = cs_type(op.response)
            if rt.endswith("?"):
                rt = rt[:-1]
            ret = f"Task<{rt}>"
            call = f'        return SendJsonAsync<{rt}>({path_expr}, HttpMethod.{op.method.capitalize()}, q, {body_arg}, options, cancellationToken);'
    else:
        ret = "Task<RawResponse>"
        call = f'        return SendRawAsync({path_expr}, HttpMethod.{op.method.capitalize()}, q, {body_arg}, options, cancellationToken);'
    return xml_doc(lines, "    ") + f"    public {ret} {name}({', '.join(params)})\n    {{\n" + query_encoder(op) + call + "\n    }\n"
