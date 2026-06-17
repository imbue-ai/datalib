// Parse the worker's discrete progress message. The worker reports
// `step x/n: Name` (optionally `… (detail)` for real sub-progress like
// qmd's embed `59%`) instead of a faked continuous percentage, since all
// we actually know is which pipeline stage is running. The UI renders
// this as an n-segment bar with x segments filled.

export type StepInfo = {
  step: number;
  total: number;
  name: string;
  detail?: string;
};

const STEP_RE = /^step (\d+)\/(\d+): (.+?)(?: \(([^)]+)\))?$/;

export function parseStep(msg: string | null | undefined): StepInfo | null {
  if (!msg) return null;
  const m = msg.match(STEP_RE);
  if (!m) return null;
  const step = Number(m[1]);
  const total = Number(m[2]);
  if (!total || step < 1) return null;
  return { step, total, name: m[3], detail: m[4] || undefined };
}
