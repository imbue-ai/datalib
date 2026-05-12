"""Tiny codegen driver: JSON Schema -> Rust / Python / TypeScript types.

v0 keeps this hand-rolled and intentionally small. We only handle the subset of
JSON Schema we actually use in `*.schema.json`: object types with
required+properties; primitives string/integer/number/boolean; nullable via
["T", "null"]; date-time formatted strings; enums of strings; $ref to local
definitions.

Custom annotations:
  * `description`           — emitted as docstring/doc-comment in all three
                              output languages.
  * `x-mapping`             — object mapping `{provider}.{kind}` to the source
                              expression that populates this column. Rendered
                              into the doc comment so per-provider semantics
                              are visible from any language.
  * `x-sql-type`            — explicit SQL column type (e.g. `VARCHAR(64)`,
                              `LONGTEXT`). Overrides the default mapping.
  * `x-primary-key` (defn)  — list of column names that form the table's
                              primary key. Triggers DDL emission for that
                              definition.
  * `x-tagged-union` (prop) — discriminated union for a property. Shape:
                              `{tag: "<sibling-field>", variants: {<value>:
                              "#/definitions/<TypeName>", ...}}`. Codegen
                              synthesizes a named union alias (parent name
                              + property name, e.g. `FeedbackContextPayload`)
                              and uses it as the property's type. The tag
                              field is a sibling, not an internal tag, so
                              Rust emits an `#[serde(untagged)]` enum.

Usage:
    python codegen.py <schema.json> --rust <out.rs>
                                    --python <out.py>
                                    --typescript <out.ts>
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def _is_nullable(t: Any) -> tuple[bool, Any]:
    if isinstance(t, list) and "null" in t:
        rest = [x for x in t if x != "null"]
        return True, rest[0] if len(rest) == 1 else rest
    return False, t


def _resolve_ref(schema: dict, ref: str) -> tuple[str, dict]:
    assert ref.startswith("#/definitions/"), ref
    name = ref.removeprefix("#/definitions/")
    return name, schema["definitions"][name]


def _tagged_union_alias_name(parent: str, prop_name: str) -> str:
    """Synthesized type name for an `x-tagged-union` property."""
    return parent + prop_name[:1].upper() + prop_name[1:]


def _tagged_union_variants(tu: dict, schema: dict) -> list[tuple[str, str]]:
    """Return [(tag_value, type_name), ...] for an x-tagged-union spec.

    Order follows insertion order of `variants` in the schema so generated
    code is deterministic.
    """
    out: list[tuple[str, str]] = []
    for tag_val, ref in tu.get("variants", {}).items():
        name, _ = _resolve_ref(schema, ref)
        out.append((tag_val, name))
    return out


def _ts_type(prop: dict, schema: dict) -> str:
    if "$ref" in prop:
        name, _ = _resolve_ref(schema, prop["$ref"])
        return name
    nullable, t = _is_nullable(prop.get("type"))
    if "enum" in prop:
        body = " | ".join(json.dumps(v) for v in prop["enum"])
        return f"({body})" + (" | null" if nullable else "")
    mapping = {
        "string": "string",
        "integer": "number",
        "number": "number",
        "boolean": "boolean",
    }
    base = mapping.get(t, "unknown")
    return base + (" | null" if nullable else "")


def _rust_type(prop: dict, schema: dict) -> str:
    if "$ref" in prop:
        name, _ = _resolve_ref(schema, prop["$ref"])
        return name
    nullable, t = _is_nullable(prop.get("type"))
    if "enum" in prop:
        base = "String"
    else:
        base = {
            "string": "String",
            "integer": "i64",
            "number": "f64",
            "boolean": "bool",
        }.get(t, "serde_json::Value")
    return f"Option<{base}>" if nullable else base


def _py_type(prop: dict, schema: dict) -> str:
    if "$ref" in prop:
        name, _ = _resolve_ref(schema, prop["$ref"])
        return name
    nullable, t = _is_nullable(prop.get("type"))
    if "enum" in prop:
        base = "str"
    else:
        base = {
            "string": "str",
            "integer": "int",
            "number": "float",
            "boolean": "bool",
        }.get(t, "object")
    return f"{base} | None" if nullable else base


def _default_sql_type(prop: dict) -> str:
    """Default SQL column type when no x-sql-type override is given.

    Strings without an explicit length default to TEXT; this is portable
    between Dolt/MySQL and SQLite. Date-time-formatted strings get a fixed
    VARCHAR(40) so they're indexable and PK-able.
    """
    _, t = _is_nullable(prop.get("type"))
    if "enum" in prop:
        return "VARCHAR(64)"
    if t == "string":
        if prop.get("format") == "date-time":
            return "VARCHAR(40)"
        return "TEXT"
    return {
        "integer": "INT",
        "number": "DOUBLE",
        "boolean": "BOOLEAN",
    }.get(t, "TEXT")


def _sql_type(prop: dict) -> str:
    return prop.get("x-sql-type") or _default_sql_type(prop)


def _emit_ddl_for(name: str, defn: dict, table: str) -> str:
    """Render a CREATE TABLE IF NOT EXISTS statement for a definition.

    Uses the SQL subset that Dolt, MySQL, and SQLite all accept (per
    `src/ingest/dump.py`). One column per property; NOT NULL on required
    columns; PRIMARY KEY clause from the definition's `x-primary-key`.
    """
    pk: list[str] = list(defn.get("x-primary-key", []))
    if not pk:
        raise ValueError(
            f"definition {name!r} (table {table!r}) has no x-primary-key — "
            "DDL cannot be emitted"
        )
    required = set(defn.get("required", []))
    lines = [f"CREATE TABLE IF NOT EXISTS {table} ("]
    parts: list[str] = []
    for prop_name, prop in defn.get("properties", {}).items():
        col_type = _sql_type(prop)
        not_null = " NOT NULL" if (prop_name in required or prop_name in pk) else ""
        parts.append(f"    {prop_name} {col_type}{not_null}")
    parts.append(f"    PRIMARY KEY ({', '.join(pk)})")
    lines.append(",\n".join(parts))
    lines.append(")")
    return "\n".join(lines)


# ----- doc-comment helpers --------------------------------------------------

_DOC_KEYS = ("description", "x-mapping")


def _doc_lines(node: dict) -> list[str]:
    """Flatten description + x-mapping (if any) into a list of plain-text
    lines suitable for embedding in any of the three output languages.
    Returns [] when the node carries neither annotation."""
    lines: list[str] = []
    desc = node.get("description")
    if desc:
        lines.extend(desc.splitlines() or [""])
    mapping = node.get("x-mapping")
    if mapping:
        if lines:
            lines.append("")
        lines.append("Per-provider mapping:")
        for k, v in mapping.items():
            v_str = "null" if v is None else str(v)
            lines.append(f"  {k}: {v_str}")
    return lines


def _py_doc(node: dict, indent: str) -> list[str]:
    """Render a Python triple-quoted docstring (one line if short, multi-
    line otherwise). Returns [] when node has no doc."""
    lines = _doc_lines(node)
    if not lines:
        return []
    if len(lines) == 1:
        return [f'{indent}"""{lines[0]}"""']
    out = [f'{indent}"""{lines[0]}']
    out.extend(f"{indent}{line}" if line else indent.rstrip() for line in lines[1:])
    out.append(f'{indent}"""')
    return out


def _rust_doc(node: dict, indent: str) -> list[str]:
    return [
        f"{indent}/// {line}" if line else f"{indent}///" for line in _doc_lines(node)
    ]


def _rust_module_doc(node: dict) -> list[str]:
    """File-level Rust comment for the schema description. Plain `//` (not
    `//!`/`///`) so the file can be `include!`d into a module without
    rustc tripping over the inner-vs-outer doc-comment placement rules."""
    return [f"// {line}" if line else "//" for line in _doc_lines(node)]


def _ts_doc(node: dict, indent: str) -> list[str]:
    lines = _doc_lines(node)
    if not lines:
        return []
    if len(lines) == 1:
        return [f"{indent}/** {lines[0]} */"]
    out = [f"{indent}/**"]
    out.extend(f"{indent} * {line}" if line else f"{indent} *" for line in lines)
    out.append(f"{indent} */")
    return out


# ----- emitters -------------------------------------------------------------


def emit_typescript(schema: dict, source_name: str) -> str:
    out = [
        "// AUTOGENERATED by schemas/codegen.py — do not edit.",
        f"// Source: {source_name}",
        "",
    ]
    schema_doc = _ts_doc(schema, "")
    if schema_doc:
        out.extend(schema_doc)
        out.append("")
    for name, defn in schema["definitions"].items():
        # Emit any synthesized tagged-union aliases ahead of the interface
        # that references them.
        for prop_name, prop in defn.get("properties", {}).items():
            tu = prop.get("x-tagged-union")
            if not tu:
                continue
            alias = _tagged_union_alias_name(name, prop_name)
            variants = _tagged_union_variants(tu, schema)
            tag = tu.get("tag", "")
            out.append(
                f"/** Discriminated union for {name}.{prop_name}, "
                f"tagged by sibling field `{tag}`. */"
            )
            body = " | ".join(t for _, t in variants)
            out.append(f"export type {alias} = {body};")
            out.append("")
        out.extend(_ts_doc(defn, ""))
        out.append(f"export interface {name} {{")
        required = set(defn.get("required", []))
        for prop_name, prop in defn.get("properties", {}).items():
            doc = _ts_doc(prop, "  ")
            if doc:
                out.extend(doc)
            opt = "" if prop_name in required else "?"
            if "x-tagged-union" in prop:
                tname = _tagged_union_alias_name(name, prop_name)
            else:
                tname = _ts_type(prop, schema)
            out.append(f"  {prop_name}{opt}: {tname};")
        out.append("}")
        out.append("")
    out.append("export const TABLES = {")
    for table, ref in schema.get("tables", {}).items():
        type_name = ref.removeprefix("#/definitions/")
        out.append(f"  {json.dumps(table)}: {json.dumps(type_name)},")
    out.append("} as const;")
    out.append("")
    return "\n".join(out)


def emit_rust(schema: dict, source_name: str) -> str:
    out = [
        "// AUTOGENERATED by schemas/codegen.py — do not edit.",
        f"// Source: {source_name}",
        "",
    ]
    schema_doc = _rust_module_doc(schema)
    if schema_doc:
        out.extend(schema_doc)
        out.append("")
    out.append("use serde::{Deserialize, Serialize};")
    out.append("")
    rust_keywords = {
        "type",
        "match",
        "ref",
        "move",
        "fn",
        "let",
        "mut",
        "use",
        "mod",
        "self",
        "super",
        "trait",
        "impl",
        "as",
        "where",
        "while",
        "loop",
    }
    ddl_emitted: list[tuple[str, str]] = []  # (table, ddl)
    columns_emitted: list[tuple[str, list[str]]] = []
    for name, defn in schema["definitions"].items():
        # Emit synthesized tagged-union enums ahead of the struct that uses
        # them. The discriminator is a *sibling* field, not internal to the
        # payload, so we emit `#[serde(untagged)]` — serde will try each
        # variant in declaration order. Variant shapes are distinct enough
        # in our schemas to make this unambiguous.
        for prop_name, prop in defn.get("properties", {}).items():
            tu = prop.get("x-tagged-union")
            if not tu:
                continue
            alias = _tagged_union_alias_name(name, prop_name)
            variants = _tagged_union_variants(tu, schema)
            tag = tu.get("tag", "")
            out.append(
                f"/// Discriminated union for `{name}.{prop_name}`, tagged by"
            )
            out.append(f"/// sibling field `{tag}`.")
            out.append("#[derive(Debug, Clone, Serialize, Deserialize)]")
            out.append("#[serde(untagged)]")
            out.append(f"pub enum {alias} {{")
            for tag_val, type_name in variants:
                # Variant name: PascalCase of the tag value.
                variant = "".join(
                    p[:1].upper() + p[1:] for p in tag_val.split("_")
                )
                out.append(f"    {variant}({type_name}),")
            out.append("}")
            out.append("")
        out.extend(_rust_doc(defn, ""))
        out.append("#[derive(Debug, Clone, Serialize, Deserialize)]")
        out.append(f"pub struct {name} {{")
        for prop_name, prop in defn.get("properties", {}).items():
            doc = _rust_doc(prop, "    ")
            if doc:
                out.extend(doc)
            field = f"r#{prop_name}" if prop_name in rust_keywords else prop_name
            if "x-tagged-union" in prop:
                ftype = _tagged_union_alias_name(name, prop_name)
            else:
                ftype = _rust_type(prop, schema)
            out.append(f"    pub {field}: {ftype},")
        out.append("}")
        out.append("")
        if "x-primary-key" in defn:
            # Find the table this definition backs (first match in tables).
            table = next(
                (
                    t
                    for t, ref in schema.get("tables", {}).items()
                    if ref == f"#/definitions/{name}"
                ),
                None,
            )
            if table:
                ddl_emitted.append((table, _emit_ddl_for(name, defn, table)))
                columns_emitted.append((table, list(defn.get("properties", {}).keys())))
    out.append("pub const TABLES: &[(&str, &str)] = &[")
    for table, ref in schema.get("tables", {}).items():
        type_name = ref.removeprefix("#/definitions/")
        out.append(f'    ("{table}", "{type_name}"),')
    out.append("];")
    out.append("")
    if ddl_emitted:
        out.append("/// Portable CREATE TABLE statements for every table this schema")
        out.append("/// defines. Same SQL subset accepted by Dolt, MySQL, and SQLite.")
        out.append("pub const DDL: &[(&str, &str)] = &[")
        for table, ddl in ddl_emitted:
            out.append(f'    ("{table}", r#"{ddl}"#),')
        out.append("];")
        out.append("")
    if columns_emitted:
        out.append("/// Column names per table, in declaration order.")
        out.append("pub const COLUMNS: &[(&str, &[&str])] = &[")
        for table, cols in columns_emitted:
            col_strs = ", ".join(f'"{c}"' for c in cols)
            out.append(f'    ("{table}", &[{col_strs}]),')
        out.append("];")
        out.append("")
    return "\n".join(out)


def emit_python(schema: dict, source_name: str) -> str:
    out = [
        f'"""AUTOGENERATED by schemas/codegen.py — do not edit. Source: {source_name}."""',
        "",
        "from __future__ import annotations",
        "",
        "from dataclasses import dataclass",
    ]
    ddl_emitted: list[tuple[str, str]] = []
    columns_emitted: list[tuple[str, list[str]]] = []
    tagged_unions: list[tuple[str, str, list[tuple[str, str]]]] = []
    for name, defn in schema["definitions"].items():
        # Note tagged unions for end-of-file emission. We can't emit them
        # inline because the variant dataclasses are defined later in the
        # file (the parent definition comes before its variant types in
        # the schema), and `Foo | Bar` is a *runtime* expression that
        # needs all names already bound. Field annotations themselves
        # work either way thanks to `from __future__ import annotations`.
        for prop_name, prop in defn.get("properties", {}).items():
            tu = prop.get("x-tagged-union")
            if not tu:
                continue
            tagged_unions.append(
                (
                    _tagged_union_alias_name(name, prop_name),
                    f"{name}.{prop_name}, tagged by sibling field `{tu.get('tag', '')}`",
                    _tagged_union_variants(tu, schema),
                )
            )
        # PEP 8: two blank lines before a top-level class.
        out.append("")
        out.append("")
        out.append("@dataclass")
        out.append(f"class {name}:")
        cls_doc = _py_doc(defn, "    ")
        if cls_doc:
            out.extend(cls_doc)
            # ruff format wants a blank line between a class docstring and
            # the first field — emit one so the generated file is stable.
            out.append("")
        props = list(defn.get("properties", {}).items())
        if not props and not cls_doc:
            out.append("    pass")
        for prop_name, prop in props:
            if "x-tagged-union" in prop:
                ptype = _tagged_union_alias_name(name, prop_name)
            else:
                ptype = _py_type(prop, schema)
            out.append(f"    {prop_name}: {ptype}")
            doc = _py_doc(prop, "    ")
            if doc:
                # In Python, field docstrings aren't standard, but multi-line
                # docstrings on a line right after the field declaration are
                # widely understood by readers and tools like Sphinx.
                out.extend(doc)
        if "x-primary-key" in defn:
            table = next(
                (
                    t
                    for t, ref in schema.get("tables", {}).items()
                    if ref == f"#/definitions/{name}"
                ),
                None,
            )
            if table:
                ddl_emitted.append((table, _emit_ddl_for(name, defn, table)))
                columns_emitted.append((table, list(defn.get("properties", {}).keys())))
    for alias, descr, variants in tagged_unions:
        out.append("")
        out.append("")
        out.append(f"# Discriminated union for {descr}.")
        body = " | ".join(t for _, t in variants)
        out.append(f"{alias} = {body}")
    out.append("")
    out.append("")
    out.append("TABLES: dict[str, str] = {")
    for table, ref in schema.get("tables", {}).items():
        type_name = ref.removeprefix("#/definitions/")
        out.append(f'    "{table}": "{type_name}",')
    out.append("}")
    if ddl_emitted:
        out.append("")
        out.append("")
        out.append(
            "# Portable CREATE TABLE statements for every table this schema defines."
        )
        out.append("# Same SQL subset accepted by Dolt, MySQL, and SQLite.")
        out.append("DDL: list[str] = [")
        for _, ddl in ddl_emitted:
            # Triple-quote so embedded newlines stay readable.
            out.append('    """')
            out.append(ddl)
            out.append('    """,')
        out.append("]")
    if columns_emitted:
        out.append("")
        out.append("")
        out.append("# Column names per table, in declaration order.")
        out.append("COLUMNS: dict[str, list[str]] = {")
        for table, cols in columns_emitted:
            out.append(f'    "{table}": [')
            for c in cols:
                out.append(f'        "{c}",')
            out.append("    ],")
        out.append("}")
    return "\n".join(out) + "\n"


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("schema", type=Path)
    ap.add_argument("--rust", type=Path)
    ap.add_argument("--python", type=Path)
    ap.add_argument("--typescript", type=Path)
    args = ap.parse_args()

    schema = json.loads(args.schema.read_text())
    source_name = args.schema.name
    if args.rust:
        args.rust.write_text(emit_rust(schema, source_name))
    if args.python:
        args.python.write_text(emit_python(schema, source_name))
    if args.typescript:
        args.typescript.write_text(emit_typescript(schema, source_name))


if __name__ == "__main__":
    main()
