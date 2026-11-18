// An input that names every step of one function, `*/render_markdown`,
// as the loader reads it (`datalib/backend/dag/src/input_pattern.rs`,
// which this mirrors and `inputPattern.test.ts` holds it to).

/// The function a pattern input names, or null for a step id.
export function patternFunction(input: string): string | null {
  if (!input.startsWith("*/")) return null;
  const fn = input.slice(2);
  return fn !== "" && !fn.includes("/") && !fn.includes("*") ? fn : null;
}

/// Whether `input`, an id or a pattern, names the step `id`.
export function namesStep(input: string, id: string): boolean {
  const fn = patternFunction(input);
  if (fn === null) return input === id;
  const slash = id.indexOf("/");
  return slash > 0 && id.slice(slash + 1) === fn;
}

/// `inputs` with each pattern replaced by the ids in `ids` it names, in
/// their order, never `own` and never an id twice.
export function expandInputs(own: string, inputs: string[], ids: string[]): string[] {
  const out: string[] = [];
  for (const input of inputs) {
    const named =
      patternFunction(input) === null
        ? [input]
        : ids.filter((id) => id !== own && namesStep(input, id));
    for (const id of named) if (!out.includes(id)) out.push(id);
  }
  return out;
}
