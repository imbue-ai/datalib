import { describe, expect, it } from "vitest";
import { fieldsWithoutSource, sourceLabel, sourceOf, sourceUrl } from "../src/components/runLogSource";

const COMMIT = "ae2d52f0f08c93fa49b82189e5715f278c517ad7";

describe("sourceOf", () => {
  it("reads the file and line out of a tracing line's fields", () => {
    expect(sourceOf('{"filename":"datalib/backend/etl/src/http.rs","line_number":510}')).toEqual({
      file: "datalib/backend/etl/src/http.rs",
      line: 510,
    });
  });

  it("drops the prefix of a crate bazel compiled from a copy", () => {
    expect(
      sourceOf('{"filename":"bazel-out/darwin_arm64-fastbuild/bin/datalib/backend/http/src/boot.rs","line_number":37}'),
    ).toEqual({ file: "datalib/backend/http/src/boot.rs", line: 37 });
  });

  it("is nothing for a plain line, bad JSON, or fields without a file", () => {
    expect(sourceOf(null)).toBeNull();
    expect(sourceOf("not json")).toBeNull();
    expect(sourceOf('{"account":"x"}')).toBeNull();
  });

  it("keeps the file when the line is missing", () => {
    expect(sourceOf('{"filename":"a/b.rs"}')).toEqual({ file: "a/b.rs", line: null });
  });
});

describe("sourceUrl", () => {
  it("links a repo file to its line at the commit", () => {
    expect(sourceUrl(COMMIT, { file: "datalib/backend/etl/src/http.rs", line: 510 })).toBe(
      `https://github.com/imbue-ai/datalib/blob/${COMMIT}/datalib/backend/etl/src/http.rs#L510`,
    );
    expect(sourceUrl(COMMIT, { file: "a/b.rs", line: null })).toBe(
      `https://github.com/imbue-ai/datalib/blob/${COMMIT}/a/b.rs`,
    );
  });

  it("does not link a third-party crate's file", () => {
    expect(sourceUrl(COMMIT, { file: "external/datalib_crates+/sqlx-core-0.9.0/src/pool/mod.rs", line: 1 })).toBeNull();
    expect(sourceUrl(COMMIT, { file: "/Users/x/.cargo/registry/src/y/z.rs", line: 1 })).toBeNull();
  });

  it("escapes what a path could carry into a URL", () => {
    expect(sourceUrl(COMMIT, { file: "a b/c#d.rs", line: 2 })).toBe(
      `https://github.com/imbue-ai/datalib/blob/${COMMIT}/a%20b/c%23d.rs#L2`,
    );
  });
});

describe("sourceLabel", () => {
  it("is file:line, or the file alone", () => {
    expect(sourceLabel({ file: "a.rs", line: 3 })).toBe("a.rs:3");
    expect(sourceLabel({ file: "a.rs", line: null })).toBe("a.rs");
  });
});

describe("fieldsWithoutSource", () => {
  it("drops the two keys the Source column shows and keeps the rest", () => {
    expect(fieldsWithoutSource('{"filename":"a.rs","line_number":3,"account":"x"}')).toBe('{"account":"x"}');
    expect(fieldsWithoutSource('{"filename":"a.rs","line_number":3}')).toBe("");
    expect(fieldsWithoutSource(null)).toBe("");
    expect(fieldsWithoutSource("not json")).toBe("not json");
  });
});
