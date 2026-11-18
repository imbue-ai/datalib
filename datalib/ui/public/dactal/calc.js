// The arithmetic behind DACTAL's inline math (`rows.[=score/total]`), in
// place of the `eval` the engine shipped with. A query can arrive in a link,
// and no filter in front of `eval` can be trusted to stop it reaching
// `Function`, so nothing here evaluates code: it parses what the query language
// can write and refuses the rest. The engine shows a refused expression as text.
//
// Values are only numbers, strings, booleans, null, undefined and arrays of
// those, so a coercion has no user code to call. Properties are never read by
// name: an index must be a number, and `.split` is the one method.
(() => {
  const TOKEN =
    /\s*(?:(\d+\.?\d*(?:[eE][+-]?\d+)?|\.\d+(?:[eE][+-]?\d+)?)|("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')|(Math\.[a-z0-9]+\b|split\b|true\b|false\b|null\b|undefined\b)|(===|==|\*\*|[-+*/%()[\],.]))/y;
  const CONSTANTS = { true: true, false: false, null: null, undefined: undefined };
  const ESCAPES = { n: "\n", t: "\t", r: "\r", b: "\b", f: "\f", v: "\v", 0: "\0" };

  function tokenize(text) {
    const tokens = [];
    TOKEN.lastIndex = 0;
    while (TOKEN.lastIndex < text.trimEnd().length) {
      const m = TOKEN.exec(text);
      if (!m) throw new Error("cannot read this expression");
      if (m[1] !== undefined) tokens.push({ kind: "number", value: Number(m[1]) });
      else if (m[2] !== undefined) tokens.push({ kind: "string", value: unquote(m[2]) });
      else if (m[3] !== undefined) tokens.push({ kind: "word", value: m[3] });
      else tokens.push({ kind: "op", value: m[4] });
    }
    return tokens;
  }

  function unquote(literal) {
    return literal
      .slice(1, -1)
      .replace(/\\u([0-9a-fA-F]{4})|\\([^])/g, (_, hex, ch) =>
        hex ? String.fromCharCode(parseInt(hex, 16)) : (ESCAPES[ch] ?? ch),
      );
  }

  function evaluate(text) {
    const tokens = tokenize(text);
    let at = 0;

    const isOp = (op) => tokens[at]?.kind === "op" && tokens[at].value === op;
    const accept = (op) => isOp(op) && ++at > 0;
    const expect = (op) => {
      if (!accept(op)) throw new Error(`expected ${op}`);
    };

    function list(close) {
      const items = [];
      if (accept(close)) return items;
      do items.push(equality());
      while (accept(","));
      expect(close);
      return items;
    }

    function equality() {
      let left = additive();
      for (;;) {
        // eslint-disable-next-line eqeqeq -- the query language's `=` is loose, as `eval` gave it
        if (accept("==")) left = left == additive();
        else if (accept("===")) left = left === additive();
        else return left;
      }
    }

    function additive() {
      let left = multiplicative();
      for (;;) {
        if (accept("+")) left = left + multiplicative();
        else if (accept("-")) left = left - multiplicative();
        else return left;
      }
    }

    function multiplicative() {
      let left = unary();
      for (;;) {
        if (accept("*")) left = left * unary();
        else if (accept("/")) left = left / unary();
        else if (accept("%")) left = left % unary();
        else return left;
      }
    }

    function unary() {
      if (accept("-")) return -unary();
      if (accept("+")) return +unary();
      const base = postfix();
      return accept("**") ? base ** unary() : base;
    }

    function postfix() {
      let value = primary();
      for (;;) {
        if (accept("[")) {
          const index = equality();
          expect("]");
          value = element(value, index);
        } else if (accept(".")) {
          value = split(value);
        } else {
          return value;
        }
      }
    }

    function element(value, index) {
      if (typeof value !== "string" && !Array.isArray(value)) throw new Error("not indexable");
      if (!Number.isInteger(index) || index < 0) throw new Error("an index is a whole number");
      return value[index];
    }

    function split(value) {
      const word = tokens[at++];
      if (word?.kind !== "word" || word.value !== "split") throw new Error("only .split");
      if (typeof value !== "string") throw new Error("only a string splits");
      expect("(");
      const args = list(")");
      if (args.length < 1 || args.length > 2 || typeof args[0] !== "string") {
        throw new Error("split takes a string and an optional limit");
      }
      return value.split(args[0], args[1]);
    }

    function primary() {
      const token = tokens[at++];
      if (!token) throw new Error("unexpected end");
      if (token.kind === "number" || token.kind === "string") return token.value;
      if (token.kind === "word") return word(token.value);
      if (token.value === "(") {
        const value = equality();
        expect(")");
        return value;
      }
      if (token.value === "[") return list("]");
      throw new Error(`unexpected ${token.value}`);
    }

    function word(name) {
      if (Object.hasOwn(CONSTANTS, name)) return CONSTANTS[name];
      if (!name.startsWith("Math.")) throw new Error(`unexpected ${name}`);
      const fn = name.slice("Math.".length);
      if (!Object.hasOwn(Math, fn) || typeof Math[fn] !== "function") {
        throw new Error(`no such function: ${name}`);
      }
      expect("(");
      return Math[fn](...list(")"));
    }

    const value = equality();
    if (at !== tokens.length) throw new Error("unexpected text after the expression");
    return value;
  }

  globalThis.dactalCalc = evaluate;
})();
