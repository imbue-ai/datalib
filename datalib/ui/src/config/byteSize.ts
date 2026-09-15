// A byte count as a person writes it: "5 MB", "512 KiB", "5_000_000".
// The same grammar the backend reads (`datalib_source_common::byte_size`),
// so what the wizard writes into the config is what a hand-editor would.

export const BYTE_UNITS = ["B", "KB", "MB", "GB"] as const;
export type ByteUnit = (typeof BYTE_UNITS)[number];

/// Decimal, not binary: the catalog's defaults and the example configs
/// already call 5_000_000 "5 MB", and the labels say KB, not KiB.
export const UNIT_BYTES: Record<ByteUnit, number> = {
  B: 1,
  KB: 1_000,
  MB: 1_000_000,
  GB: 1_000_000_000,
};

/// What an empty control starts on.
export const DEFAULT_BYTE_UNIT: ByteUnit = "KB";

export type ParsedByteSize = {
  bytes: number;
  /// The unit as written, when it is one the control offers — so a
  /// stored "5000 KB" opens as 5000 KB rather than 5 MB. Null for a
  /// bare number or a binary unit (KiB, MiB, …).
  unit: ByteUnit | null;
  amount: number;
};

const GRAMMAR = /^([0-9_]*\.?[0-9_]*)\s*([a-zA-Z]*)$/;

/// A number with optional `_` separators, then an optional unit: `B`,
/// or `K`/`M`/`G`/`T` with `B` (×1000) or `iB` (×1024), any case. A
/// bare number is bytes. Null for anything else.
export function parseByteSize(text: string | number): ParsedByteSize | null {
  if (typeof text === "number") {
    return Number.isFinite(text) && text >= 0 ? { bytes: text, unit: null, amount: text } : null;
  }
  const m = GRAMMAR.exec(text.trim());
  if (!m) return null;
  const digits = m[1].replace(/_/g, "");
  if (digits === "" || digits === ".") return null;
  const amount = Number(digits);
  if (!Number.isFinite(amount)) return null;
  const multiplier = unitMultiplier(m[2]);
  if (multiplier === null) return null;
  const upper = m[2].toUpperCase();
  const unit = (BYTE_UNITS as readonly string[]).includes(upper) ? (upper as ByteUnit) : null;
  return { bytes: Math.round(amount * multiplier), unit, amount };
}

function unitMultiplier(unit: string): number | null {
  const u = unit.toLowerCase();
  if (u === "" || u === "b") return 1;
  const power = { k: 1, m: 2, g: 3, t: 4 }[u[0]];
  if (power === undefined) return null;
  const rest = u.slice(1);
  const base = rest === "" || rest === "b" ? 1000 : rest === "ib" ? 1024 : null;
  return base === null ? null : base ** power;
}

/// The largest unit that divides `bytes` evenly, so a stored value always
/// shows as a whole number — 5_000_000 as "5 MB", 5_242_880 (a hand-edited
/// 5 MiB) as "5242880 B" rather than "5.24288 MB".
export function splitBytes(bytes: number): { amount: number; unit: ByteUnit } {
  if (!Number.isFinite(bytes) || bytes <= 0) return { amount: bytes, unit: DEFAULT_BYTE_UNIT };
  for (const unit of [...BYTE_UNITS].reverse()) {
    if (bytes % UNIT_BYTES[unit] === 0) return { amount: bytes / UNIT_BYTES[unit], unit };
  }
  return { amount: bytes, unit: "B" };
}

export function joinBytes(amount: number, unit: ByteUnit): number {
  return Math.round(amount * UNIT_BYTES[unit]);
}

/// "5 MB" — the form the wizard writes into the config.
export function formatBytes(bytes: number): string {
  const { amount, unit } = splitBytes(bytes);
  return `${amount} ${unit}`;
}
