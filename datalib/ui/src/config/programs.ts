// The sources the config's programs offer "Add source": each program its
// `[[programs]]` lists with `describe` answers `--describe` with sources
// in a package manifest's shape (docs/dev/step_protocol.md §"Describe"),
// which `GET /api/programs` serves. `datalib-step` is one such program.

import { shallowRef } from "vue";
import { fetchPrograms, type ProgramDescription, type ProgramsResponse } from "@/api";
import { setProgramDeclarations, setProgramEntries, type CatalogEntry } from "./catalog";
import { entryOfSource } from "./packages";

/// A program's sources as catalog entries, each writing its steps as the
/// program's word and then the source's command for the step's function.
export function entriesOfDescription(command: string, d: ProgramDescription): CatalogEntry[] {
  return d.sources.map((s) => ({
    ...entryOfSource(s, ""),
    program: { command, commands: s.commands },
  }));
}

export function catalogEntriesOf(res: ProgramsResponse): CatalogEntry[] {
  return res.programs.flatMap((p) =>
    p.description ? entriesOfDescription(p.command, p.description) : [],
  );
}

/// Put every program's sources, fan-ins and locks in the catalog. A fan-in
/// or a lock two programs both describe is the first one's.
export function useDescriptions(res: ProgramsResponse): void {
  setProgramEntries(catalogEntriesOf(res));
  const described = res.programs.flatMap((p) =>
    p.description ? [{ command: p.command, d: p.description }] : [],
  );
  const fanIns = described.flatMap(({ command, d }) =>
    (d.fan_ins ?? []).map((f) => ({
      ...f,
      command: { ...f.command, command: `${command} ${f.command.command.trim()}` },
      applet: f.applet ?? null,
    })),
  );
  const unique = fanIns.filter(
    (f, i) => fanIns.findIndex((g) => g.group === f.group && g.function === f.function) === i,
  );
  const locks = Object.assign({}, ...described.map(({ d }) => d.locks ?? {}).reverse());
  setProgramDeclarations(unique, locks);
}

/// The programs as the server last described them; `null` before the
/// first answer.
export const programsState = shallowRef<ProgramsResponse | null>(null);

/// Ask the server again, and put the programs' sources in the catalog.
export async function refreshPrograms(): Promise<ProgramsResponse | null> {
  try {
    const res = await fetchPrograms();
    programsState.value = res;
    useDescriptions(res);
    return res;
  } catch {
    return programsState.value;
  }
}
