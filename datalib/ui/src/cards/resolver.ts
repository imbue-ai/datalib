// What a chip names, asked once for the whole app (docs/dev/plans/chips.md
// § "One resolver"). Every surface that draws chips — a document, a grid
// cell — asks here: questions drawn in the same pass go out as one
// request, answers are kept, and a change a person makes forgets the
// keys it touched and tells every surface to draw them again.
//
// A key is forgotten rather than updated in place: the answer after an
// edit is whatever the server says next, and an answer to a question
// asked before the edit is dropped when it lands. An answer that moves
// on its own, like a step's status, is revalidated instead: asked again
// with the old one still drawn, and a surface told only if it changed.

export type Listener = (keys: ReadonlySet<string>) => void;

export class Resolver<V> {
  private readonly known = new Map<string, V>();
  /// The batch each key's question went out in, until it lands.
  private readonly inflight = new Map<string, Promise<void>>();
  /// Bumped by `forget`: an answer for an older generation is stale.
  private readonly generation = new Map<string, number>();
  private readonly listeners = new Set<Listener>();
  private next: { keys: string[]; done: Promise<void> } | null = null;

  constructor(
    private readonly fetch: (keys: string[]) => Promise<Map<string, V>>,
    private readonly onError: (e: Error) => void,
  ) {}

  /** The answer for `key`, if there is one yet. */
  get(key: string): V | undefined {
    return this.known.get(key);
  }

  /** The answer so far, and the question asked if it has not been: what a
   *  surface calls as it draws, then draws again when told. */
  lookup(key: string): V | undefined {
    this.want(key);
    return this.known.get(key);
  }

  /** Ask about every key that has no answer and no question out, then
   *  wait for all of them. A key whose question failed stays unknown. */
  async ask(keys: Iterable<string>): Promise<void> {
    const waits: Promise<void>[] = [];
    for (const key of keys) {
      this.want(key);
      const wait = this.inflight.get(key);
      if (wait) waits.push(wait);
    }
    await Promise.all(waits);
  }

  /** Drop what is known about `keys` and tell every surface; the next
   *  draw asks again. Call it after any edit that changes an answer. */
  forget(keys: Iterable<string>): void {
    const dropped = new Set<string>();
    for (const key of keys) {
      this.known.delete(key);
      this.inflight.delete(key);
      this.generation.set(key, (this.generation.get(key) ?? 0) + 1);
      dropped.add(key);
    }
    if (dropped.size > 0) this.notify(dropped);
  }

  /** Ask again about every key that has an answer, keeping each answer
   *  until its new one lands; subscribers hear only of keys whose answer
   *  changed, or vanished. */
  revalidate(): void {
    for (const key of this.known.keys()) {
      if (this.inflight.has(key)) continue;
      const batch = this.batch();
      batch.keys.push(key);
      this.inflight.set(key, batch.done);
    }
  }

  /** Be told which keys' answers changed; returns the way to stop. */
  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private want(key: string): void {
    if (this.known.has(key) || this.inflight.has(key)) return;
    const batch = this.batch();
    batch.keys.push(key);
    this.inflight.set(key, batch.done);
  }

  /// The batch questions join until the current task ends, so one draw
  /// pass is one request.
  private batch(): { keys: string[]; done: Promise<void> } {
    if (this.next) return this.next;
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const keys: string[] = [];
    const done: Promise<void> = gate.then((): Promise<void> => this.run(keys, done));
    this.next = { keys, done };
    setTimeout(() => {
      this.next = null;
      release();
    }, 0);
    return this.next;
  }

  private async run(keys: string[], done: Promise<void>): Promise<void> {
    const asked = keys.map((k) => this.generation.get(k) ?? 0);
    try {
      const answers = await this.fetch(keys);
      const landed = new Set<string>();
      keys.forEach((key, i) => {
        if ((this.generation.get(key) ?? 0) !== asked[i]) return;
        const before = this.known.get(key);
        const v = answers.get(key);
        if (v === undefined) {
          // Named nothing any more: a group removed from the config.
          if (before !== undefined && this.known.delete(key)) landed.add(key);
          return;
        }
        this.known.set(key, v);
        if (before === undefined || JSON.stringify(before) !== JSON.stringify(v)) landed.add(key);
      });
      if (landed.size > 0) this.notify(landed);
    } catch (e) {
      this.onError(e as Error);
    } finally {
      for (const key of keys) if (this.inflight.get(key) === done) this.inflight.delete(key);
    }
  }

  private notify(keys: ReadonlySet<string>): void {
    for (const listener of this.listeners) listener(keys);
  }
}
