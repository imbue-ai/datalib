"""Emit a bare `.proto` file from a compiled FileDescriptorSet.

A descriptor set (`protoc --descriptor_set_out`) holds a schema's wire
format — message and field names, field numbers, types, labels, oneofs,
enums, reserved ranges — and nothing else: comments and layout never
make it in. Printing a schema back out of one is how we carry a
third-party format whose source file we cannot vendor (see
`datalib/backend/signal-backup/proto/PROTO_PROVENANCE.md`). What comes
out is an exact wire-compatible schema with no upstream prose.

Standard library only, like every script the build runs: pyright checks
this directory without a virtualenv, so the descriptor is decoded here
rather than through the `protobuf` package.

Usage:
    proto_from_descriptor.py <descriptor.pb> <out_dir> [<root>...]

Each file in the set is written to `<out_dir>/<basename>`. With roots
(fully qualified message names, e.g. `signal.backup.Frame`), only the
messages and enums reachable from them are kept.
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass, field

# --- protobuf wire decoding -------------------------------------------

VARINT, FIXED64, LENGTH, FIXED32 = 0, 1, 2, 5


def read_varint(buf: bytes, i: int) -> tuple[int, int]:
    value = shift = 0
    while True:
        b = buf[i]
        i += 1
        value |= (b & 0x7F) << shift
        if not b & 0x80:
            return value, i
        shift += 7


def decode(buf: bytes) -> list[tuple[int, int, int | bytes]]:
    """Every (field number, wire type, value) in a message, in order."""
    out: list[tuple[int, int, int | bytes]] = []
    i = 0
    while i < len(buf):
        tag, i = read_varint(buf, i)
        number, wire = tag >> 3, tag & 7
        if wire == VARINT:
            value, i = read_varint(buf, i)
            out.append((number, wire, value))
        elif wire == LENGTH:
            size, i = read_varint(buf, i)
            out.append((number, wire, buf[i : i + size]))
            i += size
        elif wire == FIXED64:
            out.append((number, wire, buf[i : i + 8]))
            i += 8
        elif wire == FIXED32:
            out.append((number, wire, buf[i : i + 4]))
            i += 4
        else:
            raise ValueError(f"unsupported wire type {wire}")
    return out


def ints(fields: list[tuple[int, int, int | bytes]], number: int) -> list[int]:
    return [v for n, _, v in fields if n == number and isinstance(v, int)]


def blobs(fields: list[tuple[int, int, int | bytes]], number: int) -> list[bytes]:
    return [v for n, _, v in fields if n == number and isinstance(v, bytes)]


def string(fields: list[tuple[int, int, int | bytes]], number: int) -> str:
    found = blobs(fields, number)
    return found[-1].decode() if found else ""


def integer(fields: list[tuple[int, int, int | bytes]], number: int) -> int | None:
    found = ints(fields, number)
    return found[-1] if found else None


# --- the descriptor.proto subset we print -------------------------------

SCALARS = {
    1: "double",
    2: "float",
    3: "int64",
    4: "uint64",
    5: "int32",
    6: "fixed64",
    7: "fixed32",
    8: "bool",
    9: "string",
    12: "bytes",
    13: "uint32",
    15: "sfixed32",
    16: "sfixed64",
    17: "sint32",
    18: "sint64",
}
TYPE_MESSAGE, TYPE_ENUM = 11, 14
LABEL_REPEATED = 3


@dataclass
class Field:
    name: str
    number: int
    label: int
    type: int
    type_name: str
    oneof_index: int | None
    proto3_optional: bool


@dataclass
class Enum:
    name: str
    values: list[tuple[str, int]]
    reserved: list[tuple[int, int]]  # inclusive


@dataclass
class Message:
    name: str
    fields: list[Field]
    nested: list[Message]
    enums: list[Enum]
    oneofs: list[str]
    reserved: list[tuple[int, int]]  # end exclusive, as descriptor.proto has it
    full_name: str = ""
    keep: bool = field(default=True)


@dataclass
class File:
    name: str
    package: str
    messages: list[Message]
    enums: list[Enum]


def parse_field(buf: bytes) -> Field:
    f = decode(buf)
    return Field(
        name=string(f, 1),
        number=integer(f, 3) or 0,
        label=integer(f, 4) or 1,
        type=integer(f, 5) or 0,
        type_name=string(f, 6),
        oneof_index=integer(f, 9),
        proto3_optional=bool(integer(f, 17)),
    )


def parse_enum(buf: bytes) -> Enum:
    e = decode(buf)
    values = []
    for v in blobs(e, 2):
        vf = decode(v)
        values.append((string(vf, 1), integer(vf, 2) or 0))
    reserved = []
    for r in blobs(e, 4):
        rf = decode(r)
        reserved.append((integer(rf, 1) or 0, integer(rf, 2) or 0))
    return Enum(string(e, 1), values, reserved)


def parse_message(buf: bytes, scope: str) -> Message:
    m = decode(buf)
    name = string(m, 1)
    full = f"{scope}.{name}"
    reserved = []
    for r in blobs(m, 9):
        rf = decode(r)
        reserved.append((integer(rf, 1) or 0, integer(rf, 2) or 0))
    return Message(
        name=name,
        fields=[parse_field(b) for b in blobs(m, 2)],
        nested=[parse_message(b, full) for b in blobs(m, 3)],
        enums=[parse_enum(b) for b in blobs(m, 4)],
        oneofs=[string(decode(b), 1) for b in blobs(m, 8)],
        reserved=reserved,
        full_name=full,
    )


def parse_set(buf: bytes) -> list[File]:
    files = []
    for fb in blobs(decode(buf), 1):
        f = decode(fb)
        package = string(f, 2)
        files.append(
            File(
                name=string(f, 1),
                package=package,
                messages=[parse_message(b, f".{package}") for b in blobs(f, 4)],
                enums=[parse_enum(b) for b in blobs(f, 5)],
            )
        )
    return files


# --- reachability ---------------------------------------------------------


def all_messages(files: list[File]) -> dict[str, Message]:
    index: dict[str, Message] = {}

    def walk(m: Message) -> None:
        index[m.full_name] = m
        for n in m.nested:
            walk(n)

    for f in files:
        for m in f.messages:
            walk(m)
    return index


def mark_reachable(files: list[File], roots: list[str]) -> None:
    index = all_messages(files)
    for m in index.values():
        m.keep = False
    todo = [f".{r}" for r in roots]
    while todo:
        name = todo.pop()
        m = index.get(name)
        if m is None:
            # An enum, or a message in a file outside the set.
            continue
        if m.keep:
            continue
        m.keep = True
        for fld in m.fields:
            if fld.type == TYPE_MESSAGE:
                todo.append(fld.type_name)
            elif fld.type == TYPE_ENUM:
                # An enum nested in a message keeps that message.
                todo.append(fld.type_name.rsplit(".", 1)[0])
    missing = [r for r in roots if f".{r}" not in index]
    if missing:
        raise SystemExit(f"root message(s) not in the descriptor set: {missing}")
    # A kept nested message needs its enclosing messages printed around it.
    for name, m in list(index.items()):
        if m.keep:
            while "." in name[1:]:
                name = name.rsplit(".", 1)[0]
                if name in index:
                    index[name].keep = True


# --- printing -------------------------------------------------------------


def type_ref(type_name: str, package: str) -> str:
    """The shortest spelling protoc resolves from anywhere in `package`."""
    prefix = f".{package}."
    return type_name.removeprefix(prefix)


def print_enum(e: Enum, indent: str, out: list[str]) -> None:
    out.append(f"{indent}enum {e.name} {{")
    for lo, hi in e.reserved:
        out.append(
            f"{indent}  reserved {lo};"
            if lo == hi
            else f"{indent}  reserved {lo} to {hi};"
        )
    for name, number in e.values:
        out.append(f"{indent}  {name} = {number};")
    out.append(f"{indent}}}")


def print_message(m: Message, package: str, indent: str, out: list[str]) -> None:
    out.append(f"{indent}message {m.name} {{")
    inner = indent + "  "
    for e in m.enums:
        print_enum(e, inner, out)
    for n in m.nested:
        if n.keep:
            print_message(n, package, inner, out)
    for lo, hi in m.reserved:
        hi -= 1
        out.append(
            f"{inner}reserved {lo};" if lo == hi else f"{inner}reserved {lo} to {hi};"
        )

    def line(fld: Field, at: str) -> str:
        if fld.type in SCALARS:
            ty = SCALARS[fld.type]
        else:
            ty = type_ref(fld.type_name, package)
        if fld.label == LABEL_REPEATED:
            ty = f"repeated {ty}"
        elif fld.proto3_optional:
            ty = f"optional {ty}"
        return f"{at}{ty} {fld.name} = {fld.number};"

    # A proto3 `optional` is a synthetic one-member oneof in the
    # descriptor; print it as the keyword, not as a oneof block.
    real_oneofs = {
        i
        for i, _ in enumerate(m.oneofs)
        if any(f.oneof_index == i and not f.proto3_optional for f in m.fields)
    }
    printed_oneofs: set[int] = set()
    for fld in sorted(m.fields, key=lambda f: f.number):
        idx = fld.oneof_index
        if idx is None or idx not in real_oneofs:
            out.append(line(fld, inner))
            continue
        if idx in printed_oneofs:
            continue
        printed_oneofs.add(idx)
        out.append(f"{inner}oneof {m.oneofs[idx]} {{")
        for member in sorted(
            (f for f in m.fields if f.oneof_index == idx), key=lambda f: f.number
        ):
            out.append(line(member, inner + "  "))
        out.append(f"{inner}}}")
    out.append(f"{indent}}}")


def render(f: File, header: str) -> str:
    out = [header.rstrip("\n"), "", 'syntax = "proto3";', "", f"package {f.package};"]
    for e in f.enums:
        out.append("")
        print_enum(e, "", out)
    for m in f.messages:
        if m.keep:
            out.append("")
            print_message(m, f.package, "", out)
    return "\n".join(out) + "\n"


HEADER = """\
// Generated by tools/proto_from_descriptor.py from a compiled descriptor:
// the wire format alone, with none of the upstream's text. Do not edit;
// see PROTO_PROVENANCE.md beside this file for how to refresh it."""


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    descriptor, out_dir, roots = argv[1], argv[2], argv[3:]
    with open(descriptor, "rb") as fh:
        files = parse_set(fh.read())
    if roots:
        mark_reachable(files, roots)
    os.makedirs(out_dir, exist_ok=True)
    for f in files:
        path = os.path.join(out_dir, os.path.basename(f.name))
        with open(path, "w") as fh:
            fh.write(render(f, HEADER))
        print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
