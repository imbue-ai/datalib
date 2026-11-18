import { beforeAll, describe, expect, it } from "vitest";
import calcSource from "../public/dactal/calc.js?raw";
import engineSource from "../public/dactal/vendor/dactal.js?raw";

/// DACTAL's inline math is reachable from a link (`/dactalView({q:"…"})`), so
/// no query may run as code. These run the real vendored engine, as the page's
/// classic scripts do, and without the page's CSP: if the engine went back to
/// `eval`, nothing here would stop the attack below but the engine itself.

type Engine = {
  load(items: object[], name: string): void;
  survey(): void;
  executeq(query: string): unknown[];
};
type Globals = typeof globalThis & { dactalCalc: (text: string) => unknown; pwned?: number };
const globals = globalThis as Globals;

let newEngine: () => Engine;

beforeAll(() => {
  new Function(calcSource)();
  newEngine = new Function(`${engineSource}; return () => new DACTAL();`)() as () => Engine;
});

function run(expression: string, row: object = { id: "a", name: "a", n: 2 }): unknown[] {
  const engine = newEngine();
  engine.load([row], "rows");
  engine.survey();
  return engine.executeq(`rows.[=${expression.replaceAll("]", "]]")}]`);
}

/// Spells `text` out of the words the engine's filter allows: it rewrites
/// `cos` to `Math.cos` even inside a string, so each letter is an index into one.
function spelled(text: string): string {
  const words = ["acos", "atan", "cbrt", "exp", "fround", "pow", "sinh"];
  const letter = (c: string) => {
    if (c === ".") return "'cos'[4]";
    if ("=1".includes(c)) return `'${c}'`;
    const word = words.find((w) => w.includes(c));
    if (!word) throw new Error(`no math word has ${c}`);
    return `'${word}'[${`Math.${word}`.indexOf(c, 5)}]`;
  };
  return [...text].map(letter).join("+");
}

describe("a query with inline math", () => {
  /// The regression: this passed the engine's filter, reached `Function` and
  /// ran, in a frame that can ask the host for any search.
  it("does not run code that passes the engine's filter", () => {
    const ctor = spelled("constructor");
    const code = spelled("this.pwned=1");
    delete globals.pwned;
    run(`[][${ctor}][${ctor}](${code})()`);
    expect(globals.pwned).toBeUndefined();
  });

  it("still does the math a query can write", () => {
    expect(run("n*3")).toEqual([6]);
    expect(run("(n+1)/2")).toEqual([1.5]);
    expect(run("sqrt(n*8)")).toEqual([4]);
    expect(run("max(1,n,3)")).toEqual([3]);
    expect(run("n%2")).toEqual([0]);
    expect(run("n**3")).toEqual([8]);
    expect(run("[5,6][1]")).toEqual([6]);
    expect(run("'1 2'.split(' ')[1]")).toEqual(["2"]);
  });

  it("reads a row's values, and filters on a comparison", () => {
    const row = { id: "a", name: "a", n: 2, kind: "x y", tags: ["p", "q"] };
    expect(run("kind.split(' ')[0]", row)).toEqual(["x"]);
    expect(run("tags[1]", row)).toEqual(["q"]);
    expect(run("n=2", row)).toEqual([true]);
    expect(run("n=3", row)).toEqual([]);
  });

  it("shows an expression it refuses as text", () => {
    expect(run("1 2")).toEqual(["1 2"]);
  });
});

describe("dactalCalc", () => {
  it("reads strings the way JSON and a person write them", () => {
    expect(globals.dactalCalc(JSON.stringify('a"b\\c\né') + "[1]")).toBe('"');
    expect(globals.dactalCalc("'it\\'s'")).toBe("it's");
    expect(globals.dactalCalc("1e3+.5")).toBe(1000.5);
    expect(globals.dactalCalc("3 - -2")).toBe(5);
  });

  /// Each is something `eval` would have done. A property is read only by a
  /// whole number, and `.split` is the only name after a dot.
  it("refuses everything that is not arithmetic", () => {
    for (const text of [
      "[]['constructor']",
      "''['length']",
      "'a'.length",
      "[].concat",
      "[].map(1)",
      "Math.constructor",
      "Math.PI",
      "Math.abs",
      "(1,2)",
      "{}",
      "a",
      "this",
      "globalThis",
      "`x`",
      "1;2",
      "1=1",
      "!1",
      "'a'[-1]",
      "'a'[0.5]",
      "5[0]",
      "[1].split(',')",
      "'a'.split(1)",
    ]) {
      expect(() => globals.dactalCalc(text), text).toThrow();
    }
  });
});
