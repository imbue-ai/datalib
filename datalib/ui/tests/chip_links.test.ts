import { describe, expect, it } from "vitest";
import { entityFromUri, handleFromUri, uriFromHandle } from "../src/cards/chipLinks";
import { renderDocument } from "../src/cards/renderDocument";

/// A chip is a link whose href names a handle (docs/dev/plans/chips.md).
/// The URI forms here are the ones `datalib_handle::Handle::to_uri` and
/// `from_uri` are tested over, so the two sides cannot drift apart
/// without one of the two suites failing.
describe("handleFromUri / uriFromHandle", () => {
  it("round-trips every kind through the URI the renderer writes", () => {
    for (const [handle, uri] of [
      ["email:riker@enterprise.org", "mailto:riker@enterprise.org"],
      ["tel:+15550123456", "tel:+15550123456"],
      ["slack:T01/U02", "slack://user?team=T01&id=U02"],
      [
        "signal_aci:0195683a-d140-87f9-bdf6-234da6d6880c",
        "datalib:handle/signal_aci/0195683a-d140-87f9-bdf6-234da6d6880c",
      ],
    ]) {
      expect(uriFromHandle(handle)).toBe(uri);
      expect(handleFromUri(uri)).toBe(handle);
    }
    expect(handleFromUri("mailto:Riker@Enterprise.org?subject=hi")).toBe(
      "email:riker@enterprise.org",
    );
    expect(handleFromUri("slack://user?id=U02&team=T01")).toBe("slack:T01/U02");
    expect(handleFromUri("https://enterprise.org/riker")).toBeNull();
    expect(handleFromUri("slack://channel?team=T01&id=C03")).toBeNull();
    expect(handleFromUri("datalib:group/slack")).toBeNull();
    expect(handleFromUri("datalib:handle/tel/+15550123456")).toBe("tel:+15550123456");
    expect(handleFromUri("datalib:handle/fax/+15550123456")).toBeNull();
    expect(handleFromUri("datalib:handle/signal_aci/0195683AD14087F9BDF6234DA6D6880C")).toBe(
      "signal_aci:0195683a-d140-87f9-bdf6-234da6d6880c",
    );
    expect(handleFromUri("datalib:handle/signal_aci/0195683a")).toBeNull();
    expect(handleFromUri("mailto:not an address")).toBeNull();
    expect(uriFromHandle("fax:+15550123456")).toBeNull();
  });
});

/// The group and step URIs `datalib_columns::Entity` writes and reads.
describe("entityFromUri", () => {
  it("reads a group and a step, and nothing else", () => {
    expect(entityFromUri("datalib:group/slack")).toEqual({ kind: "group", id: "slack" });
    expect(entityFromUri("datalib:step/slack/ingest")).toEqual({
      kind: "step",
      id: "slack/ingest",
    });
    expect(entityFromUri("datalib:group/")).toBeNull();
    expect(entityFromUri("datalib:handle/tel/+15550123456")).toBeNull();
    expect(entityFromUri("mailto:riker@enterprise.org")).toBeNull();
  });
});

describe("the chipLinks markdown plugin", () => {
  const page = (md: string) => {
    const div = document.createElement("div");
    div.innerHTML = renderDocument(md, null).html;
    return div;
  };

  it("marks a link whose href names a handle, and keeps its title", () => {
    const div = page(
      '## [Will Riker](mailto:riker@enterprise.org "Will Riker <riker@enterprise.org>") <time class="msg-ts">x</time>\n\n' +
        'Ask [@data](slack://user?team=T01&id=U02 "Data (slack:T01/U02)") about it.\n',
    );
    const chips = Array.from(div.querySelectorAll<HTMLAnchorElement>("a.chip"));
    expect(chips.map((a) => a.dataset.handle)).toEqual([
      "email:riker@enterprise.org",
      "slack:T01/U02",
    ]);
    expect(chips[0].getAttribute("title")).toBe("Will Riker <riker@enterprise.org>");
    expect(chips[0].getAttribute("href")).toBe("mailto:riker@enterprise.org");
    expect(chips[0].textContent).toBe("Will Riker");
    expect(chips[1].getAttribute("href")).toBe("slack://user?team=T01&id=U02");
  });

  it("marks a link naming a group or a step with its URI", () => {
    const div = page(
      "Stored by [Slack](datalib:group/slack), fetched by [ingest](datalib:step/slack/ingest).\n",
    );
    const chips = Array.from(div.querySelectorAll<HTMLAnchorElement>("a.chip"));
    expect(chips.map((a) => a.dataset.entity)).toEqual([
      "datalib:group/slack",
      "datalib:step/slack/ingest",
    ]);
    expect(chips.every((a) => a.dataset.handle === undefined)).toBe(true);
  });

  /** A bare address in running text is a link linkify made, not one the
   *  renderer wrote; a signature stays a signature. */
  it("leaves a linkified bare address and every other link alone", () => {
    const div = page(
      "Write to riker@enterprise.org, or see [the crew](https://enterprise.org/crew) and [Q](mailto:q@continuum.org).\n",
    );
    const links = Array.from(div.querySelectorAll<HTMLAnchorElement>("a"));
    expect(links.map((a) => a.className)).toEqual(["", "", "chip"]);
    expect(links[0].getAttribute("href")).toBe("mailto:riker@enterprise.org");
    expect(links[0].dataset.handle).toBeUndefined();
  });

  /** Plain text is escaped by the renderer before it reaches here, so a
   *  typed `[x](mailto:…)` arrives with its brackets escaped and is text. */
  it("does not make a chip of escaped link syntax", () => {
    const div = page("\\[Captain\\](mailto:phisher@x.test)\n");
    expect(div.querySelector("a.chip")).toBeNull();
    expect(div.textContent?.trim()).toBe("[Captain](mailto:phisher@x.test)");
  });
});
