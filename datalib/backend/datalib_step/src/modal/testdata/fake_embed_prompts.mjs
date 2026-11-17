// A stand-in for `datalib-step embed-prompts` in the fake Modal's
// Sandboxes: each `{"hash","prompt"}` line gets the vector
// [prompt length, 1], in order.
import { createInterface } from "node:readline";

for await (const line of createInterface({ input: process.stdin })) {
  if (!line) continue;
  const { hash, prompt } = JSON.parse(line);
  process.stdout.write(JSON.stringify({ hash, vector: [prompt.length, 1], cut: false }) + "\n");
}
