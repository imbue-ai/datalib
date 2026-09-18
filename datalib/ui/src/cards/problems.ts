// The words the document view puts on a problem: the reason in a
// sentence a person would use, with the field where there is one. The
// vocabulary is `datalib_problems`'s; a word this build does not know
// is shown as itself rather than hidden.
import type { DocProblem } from "@/api";

export function problemLabel(p: Pick<DocProblem, "field" | "reason" | "rule">): string {
  const what = p.field ? `\`${p.field}\`` : "this record";
  switch (p.reason) {
    case "undeserializable":
      return `${what} could not be read`;
    case "no_identity":
      return `${what} has no identity, so the record was dropped`;
    case "coercion_failed":
      return `${what} was not in the form expected and was left empty`;
    case "uncovered_type":
      return `${what} has a type the renderer does not handle and was left empty`;
    case "deliberate_loss":
      return `${what} was trimmed by the rule ${p.rule ?? "?"}`;
    case "noted":
      return `${what}: a finding, nothing lost`;
    default:
      return `${what}: ${p.reason as string}`;
  }
}
