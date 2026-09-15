// A byte count as a person reads it: an amount and a unit. The config
// stores plain bytes; only the wizard's `bytes` control speaks in units.

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
