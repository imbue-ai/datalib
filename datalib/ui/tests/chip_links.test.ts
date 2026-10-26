import { describe, expect, it } from "vitest";
import {
  contactFromUri,
  entityFromUri,
  handleFromUri,
  uriFromHandle,
} from "../src/cards/chipLinks";
import { renderDocument } from "../src/cards/renderDocument";

/// A chip is a link whose href names a handle (docs/dev/chips.md).
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
      ["facebook:name/Jean-Luc Picard", "datalib:handle/facebook/name/Jean-Luc%20Picard"],
      [
        "facebook:name/Beverly Crusher-Howard ☕",
        "datalib:handle/facebook/name/Beverly%20Crusher-Howard%20%E2%98%95",
      ],
      ["facebook:deleted/1000000002", "datalib:handle/facebook/deleted/1000000002"],
      ["linkedin:jlpicard", "https://www.linkedin.com/in/jlpicard"],
      ["google:name/Geordi La Forge", "datalib:handle/google/name/Geordi%20La%20Forge"],
      ["linkedin:jürgen-müller-1a2b", "https://www.linkedin.com/in/j%C3%BCrgen-m%C3%BCller-1a2b"],
      ["github:data-soong", "datalib:handle/github/data-soong"],
      ["github:dependabot[bot]", "datalib:handle/github/dependabot%5Bbot%5D"],
      ["gitlab:gitlab.com/jlpicard", "datalib:handle/gitlab/gitlab.com/jlpicard"],
      [
        "notion:00000001-1701-4d00-8000-000000000001",
        "datalib:handle/notion/00000001-1701-4d00-8000-000000000001",
      ],
      ["whatsapp:100000000000108", "datalib:handle/whatsapp/100000000000108"],
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
    expect(handleFromUri("datalib:handle/github/Data-Soong")).toBe("github:data-soong");
    expect(handleFromUri("datalib:handle/github/data%20soong")).toBeNull();
    expect(handleFromUri("datalib:handle/gitlab/jlpicard")).toBeNull();
    expect(handleFromUri("datalib:handle/notion/not-a-uuid")).toBeNull();
    expect(handleFromUri("datalib:handle/whatsapp/12ab")).toBeNull();
    expect(handleFromUri("datalib:handle/facebook/name/Q%2")).toBeNull();
    expect(handleFromUri("datalib:handle/facebook/name/%20%20")).toBeNull();
    expect(handleFromUri("datalib:handle/facebook/deleted/12a")).toBeNull();
    expect(handleFromUri("datalib:handle/facebook/other/Q")).toBeNull();
    expect(handleFromUri("datalib:handle/google/name/%20")).toBeNull();
    expect(handleFromUri("datalib:handle/google/other/Q")).toBeNull();
    expect(handleFromUri("http://fr.linkedin.com/in/JLPicard/?trk=x")).toBe("linkedin:jlpicard");
    expect(handleFromUri("https://www.linkedin.com/in/j%c3%bcrgen-m%c3%bcller-1a2b")).toBe(
      "linkedin:jürgen-müller-1a2b",
    );
    expect(handleFromUri("https://www.linkedin.com/feed/update/urn:li:share:1")).toBeNull();
    expect(handleFromUri("https://www.linkedin.com.evil.test/in/jlpicard")).toBeNull();
    expect(handleFromUri("https://www.linkedin.com/in/jl/picard")).toBeNull();
    expect(handleFromUri("https://www.linkedin.com/in/")).toBeNull();
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

/// The URI `datalib_contacts::contact_uri` writes for one of your contacts.
describe("contactFromUri", () => {
  it("reads a contact's id, and nothing else", () => {
    expect(contactFromUri("datalib:contact/7f1c-uuid")).toBe("7f1c-uuid");
    expect(contactFromUri("datalib:contact/")).toBeNull();
    expect(contactFromUri("datalib:group/slack")).toBeNull();
    expect(handleFromUri("datalib:contact/7f1c-uuid")).toBeNull();
    expect(entityFromUri("datalib:contact/7f1c-uuid")).toBeNull();
  });
});

describe("the chipLinks markdown plugin", () => {
  const page = (md: string) => {
    const div = document.createElement("div");
    div.innerHTML = renderDocument(md).html;
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
