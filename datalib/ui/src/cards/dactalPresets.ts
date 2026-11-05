// The canned queries the DACTAL card offers for the sources a config
// has: each one a Datalib search that loads a working set and a DACTAL
// query over it. Pure, so the card only fetches the config and posts
// what this returns to its frame.

import type { ConfiguredGroup } from "@/config/sourceSteps";

export type DactalPreset = {
  /// What the chips for this preset are filed under: "Slack", or with
  /// several Slack sources, "Slack: " and the source's name.
  group: string;
  label: string;
  /// The Datalib search that loads the working set.
  load: string;
  /// The DACTAL query run over it.
  dq: string;
  /// How many rows the load asks for, newest first.
  limit: number;
};

/// The most rows a preset loads: the most the card lets its frame ask for.
export const PRESET_LIMIT = 2000;

// The kinds slack_render writes (`slack_render/src/render/render.rs`),
// bracketed because DACTAL reads a space as syntax.
const MESSAGE = "kind=[Slack Message]";
const THREAD = "kind=[Slack Thread]";
const REACTION = "kind=[Slack Reaction]";

const SLACK: { label: string; dq: string }[] = [
  { label: "Messages per channel", dq: `rows:${MESSAGE}/conversation_name#-count` },
  { label: "Who posts most", dq: `rows:${MESSAGE}/author#-count` },
  { label: "Threads, newest first", dq: `rows:${THREAD}#-touched_at` },
  { label: "Reactions by emoji", dq: `rows:${REACTION}/name#-count` },
  { label: "Who reacts most", dq: `rows:${REACTION}/author#-count` },
];

/// One set of Slack presets per Slack source in the config, each loading
/// that source's rows; none when the config has no Slack source.
export function presetsFor(groups: ConfiguredGroup[]): DactalPreset[] {
  const slack = groups.filter((g) => g.type === "slack" && g.id);
  return slack.flatMap((g) =>
    SLACK.map((p) => ({
      group: slack.length === 1 ? "Slack" : `Slack: ${g.name || g.id}`,
      label: p.label,
      load: `source:${g.id}`,
      dq: p.dq,
      limit: PRESET_LIMIT,
    })),
  );
}
