"""Intermediate representation of `api/openapi.json` for the server-SDK emitters.

Schemas become flat `Model`s (allOf merged, inline objects lifted to named models), string
enums become `Enum`s and every JSON operation becomes an `Operation`. Only the subset of
OpenAPI 3.1 the Aurix spec uses is supported; anything else raises so a spec change is
noticed at generation time instead of producing a silently wrong client.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Tuple

JSON_METHODS = ("get", "post", "put", "patch", "delete")

# Friendlier names for a few lifted inline objects.
# `Error` would shadow the built-in error types of Go/TS/C#; the nested detail object gets a
# readable name instead of `ErrorEnvelopeError`.
RENAMES = {"Error": "ErrorEnvelope", "ErrorEnvelopeError": "ErrorDetail"}


@dataclass
class TypeRef:
    """A resolved type. `kind` is one of string, integer, number, boolean, any, array, map,
    ref, enum, union."""

    kind: str
    nullable: bool = False
    format: Optional[str] = None
    items: Optional["TypeRef"] = None  # array / map value
    ref: Optional[str] = None  # model or enum name
    variants: List["TypeRef"] = field(default_factory=list)  # union

    def with_nullable(self, nullable: bool) -> "TypeRef":
        return TypeRef(self.kind, nullable or self.nullable, self.format, self.items, self.ref, self.variants)


@dataclass
class Field:
    name: str
    type: TypeRef
    required: bool
    description: str = ""


@dataclass
class Model:
    name: str
    fields: List[Field]
    description: str = ""
    additional: Optional[TypeRef] = None  # open object (additionalProperties)


@dataclass
class Enum:
    name: str
    values: List[str]
    description: str = ""


@dataclass
class Param:
    name: str
    type: TypeRef
    required: bool
    description: str = ""


@dataclass
class Operation:
    id: str
    method: str
    path: str
    tag: str
    summary: str
    description: str
    path_params: List[Param]
    query_params: List[Param]
    body: Optional[TypeRef]
    body_required: bool
    response: Optional[TypeRef]  # None → no body (204 / empty)
    response_kind: str  # json | text | binary
    response_media: List[str]
    security: List[str]
    permissions: List[str]


@dataclass
class Api:
    version: str
    title: str
    models: Dict[str, Model]
    enums: Dict[str, Enum]
    operations: List[Operation]
    skipped: List[Tuple[str, str]]  # (operationId, reason)


def pascal(s: str) -> str:
    parts = re.split(r"[^0-9a-zA-Z]+", s)
    out = "".join(p[:1].upper() + p[1:] for p in parts if p)
    return out


def snake(s: str) -> str:
    s = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", s)
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s)
    return re.sub(r"[^0-9a-zA-Z]+", "_", s).lower().strip("_")


def camel(s: str) -> str:
    p = pascal(s)
    return p[:1].lower() + p[1:]


class Loader:
    def __init__(self, spec: dict):
        self.spec = spec
        self.schemas = spec["components"]["schemas"]
        self.models: Dict[str, Model] = {}
        self.enums: Dict[str, Enum] = {}
        self._building: set = set()

    # ---- schema resolution -------------------------------------------------------------

    def ref_name(self, ref: str) -> str:
        prefix = "#/components/schemas/"
        if not ref.startswith(prefix):
            raise ValueError(f"unsupported $ref {ref}")
        return ref[len(prefix) :]

    def ensure_named(self, schema_name: str) -> TypeRef:
        """Resolve a components/schemas entry into a model or enum and return a ref to it."""
        name = RENAMES.get(schema_name, schema_name)
        if name in self.models:
            return TypeRef("ref", ref=name)
        if name in self.enums:
            return TypeRef("enum", ref=name)
        if name in self._building:
            return TypeRef("ref", ref=name)  # recursive reference; the model is being defined
        schema = self.schemas[schema_name]
        if schema.get("type") == "string" and "enum" in schema:
            self.enums[name] = Enum(name, list(schema["enum"]), schema.get("description", ""))
            return TypeRef("enum", ref=name)
        self._building.add(name)
        try:
            t = self.resolve(schema, name)
        finally:
            self._building.discard(name)
        if t.kind in ("ref", "enum") and t.ref == name:
            return t
        # A named schema that is not an object (e.g. plain string): alias by wrapping in a
        # model-less ref is not representable; emit as its underlying type everywhere.
        raise ValueError(f"schema {name} resolves to a non-object {t.kind}; not supported")

    def merge_all_of(self, parts: List[dict], name: str) -> Model:
        fields: Dict[str, Field] = {}
        additional = None
        desc = ""
        for i, part in enumerate(parts):
            if "$ref" in part:
                sub = self.ensure_named(self.ref_name(part["$ref"]))
                if sub.kind != "ref":
                    raise ValueError(f"allOf part {part['$ref']} in {name} is not an object")
                model = self.models[sub.ref]
                for f in model.fields:
                    fields[f.name] = f
                additional = additional or model.additional
                desc = desc or model.description
            else:
                model = self.object_model(part, name, register=False)
                for f in model.fields:
                    fields[f.name] = f
                additional = additional or model.additional
                desc = desc or model.description
        return Model(name, list(fields.values()), desc, additional)

    def object_model(self, schema: dict, name: str, register: bool = True) -> Model:
        name = RENAMES.get(name, name)
        if register:
            name = self.unique_name(name)
        props = schema.get("properties", {})
        required = set(schema.get("required", []))
        fields = []
        for pname, pschema in props.items():
            t = self.resolve(pschema, f"{name}{pascal(pname)}")
            fields.append(Field(pname, t, pname in required, pschema.get("description", "") if isinstance(pschema, dict) else ""))
        additional = None
        ap = schema.get("additionalProperties")
        if ap is True or (ap == {} and not props):
            additional = TypeRef("any")
        elif isinstance(ap, dict) and ap:
            additional = self.resolve(ap, f"{name}Value")
        model = Model(name, fields, schema.get("description", ""), additional)
        if register:
            self.models[name] = model
        return model

    def unique_name(self, name: str) -> str:
        """Inline objects must not shadow named schemas (or each other)."""
        if name in self._building:
            return name
        if name not in self.models and name not in self.schemas and name not in self.enums:
            return name
        i = 2
        while f"{name}{i}" in self.models or f"{name}{i}" in self.schemas:
            i += 1
        return f"{name}{i}"

    def resolve(self, schema: dict, name_hint: str) -> TypeRef:
        """Resolve any schema node. `name_hint` names inline objects/enums."""
        if schema == {} or schema is True:
            return TypeRef("any")
        if "$ref" in schema:
            return self.ensure_named(self.ref_name(schema["$ref"]))
        if "allOf" in schema:
            model = self.merge_all_of(schema["allOf"], name_hint)
            if schema.get("description"):
                model.description = schema["description"]
            self.models[name_hint] = model
            return TypeRef("ref", ref=name_hint)
        if "oneOf" in schema or "anyOf" in schema:
            variants = schema.get("oneOf") or schema.get("anyOf")
            nullable = any(v.get("type") == "null" for v in variants)
            rest = [v for v in variants if v.get("type") != "null"]
            if len(rest) == 1:
                return self.resolve(rest[0], name_hint).with_nullable(nullable)
            resolved = [self.resolve(v, f"{name_hint}Variant{i}") for i, v in enumerate(rest)]
            return TypeRef("union", nullable=nullable, variants=resolved)
        typ = schema.get("type")
        nullable = False
        if isinstance(typ, list):
            nullable = "null" in typ
            non_null = [t for t in typ if t != "null"]
            if len(non_null) != 1:
                raise ValueError(f"unsupported type list {typ} at {name_hint}")
            typ = non_null[0]
        if typ is None:
            # bare {"description": ...} or {"example": ...}
            if set(schema.keys()) <= {"description", "example", "default", "deprecated"}:
                return TypeRef("any")
            if "properties" in schema:
                typ = "object"
            else:
                raise ValueError(f"schema without type at {name_hint}: {json.dumps(schema)[:120]}")
        if typ == "string":
            if "enum" in schema and len(schema["enum"]) > 1:
                values = list(schema["enum"])
                # Reuse an identical named enum when one exists.
                for ename, e in self.enums.items():
                    if e.values == values:
                        return TypeRef("enum", nullable=nullable, ref=ename)
                self.enums[name_hint] = Enum(name_hint, values, schema.get("description", ""))
                return TypeRef("enum", nullable=nullable, ref=name_hint)
            return TypeRef("string", nullable=nullable, format=schema.get("format"))
        if typ in ("integer", "number", "boolean"):
            return TypeRef(typ, nullable=nullable, format=schema.get("format"))
        if typ == "array":
            items = self.resolve(schema.get("items", {}), f"{name_hint}Item")
            return TypeRef("array", nullable=nullable, items=items)
        if typ == "object":
            props = schema.get("properties")
            ap = schema.get("additionalProperties")
            if not props:
                if isinstance(ap, dict) and ap:
                    return TypeRef("map", nullable=nullable, items=self.resolve(ap, f"{name_hint}Value"))
                return TypeRef("map", nullable=nullable, items=TypeRef("any"))
            return TypeRef("ref", nullable=nullable, ref=self.object_model(schema, name_hint).name)
        raise ValueError(f"unsupported type {typ} at {name_hint}")

    # ---- operations --------------------------------------------------------------------

    def load_operations(self) -> Tuple[List[Operation], List[Tuple[str, str]]]:
        ops: List[Operation] = []
        skipped: List[Tuple[str, str]] = []
        for path, item in self.spec["paths"].items():
            shared = item.get("parameters", [])
            for method in JSON_METHODS:
                if method not in item:
                    continue
                op = item[method]
                op_id = op["operationId"]
                success = {c: r for c, r in op["responses"].items() if c.startswith("2")}
                if not success:
                    skipped.append((op_id, f"no 2xx response ({', '.join(op['responses'])})"))
                    continue
                code, resp = sorted(success.items())[0]
                content = resp.get("content", {})
                if "text/event-stream" in content:
                    skipped.append((op_id, "server-sent events; use the SSE helper"))
                    continue
                response: Optional[TypeRef] = None
                kind = "json"
                media = list(content.keys())
                if not content:
                    response = None
                elif "application/json" in content:
                    response = self.resolve(content["application/json"].get("schema", {}), f"{pascal(op_id)}Response")
                    if response.kind == "map" and response.items and response.items.kind == "any" and len(content) == 1 and op_id == "openapi":
                        pass
                else:
                    kind = "text" if all(m.startswith("text/") or m == "application/x-subrip" for m in media) else "binary"
                path_params: List[Param] = []
                query_params: List[Param] = []
                for prm in shared + op.get("parameters", []):
                    if "$ref" in prm:
                        raise ValueError(f"$ref parameters not supported ({op_id})")
                    t = self.resolve(prm.get("schema", {}), f"{pascal(op_id)}{pascal(prm['name'])}")
                    p = Param(prm["name"], t, prm.get("required", False) or prm["in"] == "path", prm.get("description", ""))
                    (path_params if prm["in"] == "path" else query_params).append(p)
                # Path parameters in URL order.
                order = re.findall(r"{([^}]+)}", path)
                path_params.sort(key=lambda p: order.index(p.name))
                if set(order) != {p.name for p in path_params}:
                    raise ValueError(f"path params mismatch in {op_id}: {order} vs {[p.name for p in path_params]}")
                body = None
                body_required = False
                rb = op.get("requestBody")
                if rb:
                    body = self.resolve(rb["content"]["application/json"]["schema"], f"{pascal(op_id)}Request")
                    body_required = rb.get("required", False)
                security = [",".join(s.keys()) for s in op.get("security", self.spec.get("security", []))]
                ops.append(
                    Operation(
                        id=op_id,
                        method=method.upper(),
                        path=path,
                        tag=(op.get("tags") or ["default"])[0],
                        summary=op.get("summary", ""),
                        description=op.get("description", ""),
                        path_params=path_params,
                        query_params=query_params,
                        body=body,
                        body_required=body_required,
                        response=response,
                        response_kind=kind,
                        response_media=media,
                        security=[s for s in security if s],
                        permissions=list(op.get("x-aurix-permissions", [])),
                    )
                )
        return ops, skipped


def load(path: Path) -> Api:
    spec = json.loads(path.read_text())
    loader = Loader(spec)
    for name in list(loader.schemas):
        loader.ensure_named(name)
    ops, skipped = loader.load_operations()
    return Api(
        version=spec["info"]["version"],
        title=spec["info"]["title"],
        models=loader.models,
        enums=loader.enums,
        operations=ops,
        skipped=skipped,
    )


if __name__ == "__main__":
    import sys

    api = load(Path(sys.argv[1]))
    print(f"{len(api.models)} models, {len(api.enums)} enums, {len(api.operations)} operations, skipped {api.skipped}")
    for name, m in api.models.items():
        print(name, [(f.name, f.type.kind, f.type.ref, f.required, f.type.nullable) for f in m.fields][:6], m.additional)
