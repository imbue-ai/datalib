<script setup lang="ts">
// Look a package up from a link and show what adding it means before
// anything is added: what it is, where it comes from, what it adds and
// runs, and that it runs as you. Used at the foot of "Add source" and in
// the Packages dialog, to add one and to review an update.
// docs/dev/packages.md.
import { computed, nextTick, onMounted, ref } from "vue";
import { lookupPackage, type PackageLookup } from "@/api";
import { formatBytes } from "@/config/bytes";
import { GALLERY, type GalleryEntry } from "@/config/packageGallery";
import { packagesState } from "@/config/packages";

const props = defineProps<{
  /// Does the add (or the update) once the person agrees. Throws with
  /// what went wrong.
  commit: (source: string, lookup: PackageLookup) => Promise<void>;
  /// Reviewing an update to a package already here: its source is
  /// looked up at once, and the review says what changes.
  current?: { source: string; sha256: string; name: string; version: string } | null;
  /// The heading over the link box.
  label?: string;
}>();

const emit = defineEmits<{
  (e: "done", lookup: PackageLookup): void;
  (e: "cancel"): void;
}>();

const source = ref(props.current?.source ?? "");
const looking = ref(false);
const error = ref<string | null>(null);
const found = ref<PackageLookup | null>(null);
const adding = ref(false);
const reviewEl = ref<HTMLElement | null>(null);

const PLATFORM_NAMES: Record<string, string> = {
  "aarch64-apple-darwin": "macOS (Apple silicon)",
  "x86_64-unknown-linux-gnu": "Linux (x86-64)",
  "aarch64-unknown-linux-gnu": "Linux (ARM64)",
  "x86_64-unknown-linux-musl": "Linux (x86-64, musl)",
  "aarch64-unknown-linux-musl": "Linux (ARM64, musl)",
};

const unchanged = computed(() => !!props.current && found.value?.sha256 === props.current.sha256);

/// The programs the package runs, each once, by the first word of
/// its commands.
const programs = computed(() => {
  const m = found.value?.manifest;
  if (!m) return [];
  const words = [
    ...m.sources.flatMap((s) => Object.values(s.commands)),
    ...m.applets.map((a) => a.command),
  ].map((c) => c.trim().split(/\s+/)[0]);
  return [...new Set(words)];
});

const adds = computed(() => {
  const m = found.value?.manifest;
  if (!m) return [];
  return [
    ...m.sources.map((s) => ({
      what: "A source type",
      name: typeof s.wizard.label === "string" ? s.wizard.label : s.type,
      where: "in Add source",
    })),
    ...m.applets.map((a) => ({ what: "An applet", name: a.id, where: "which can add cards" })),
  ];
});

const buildLine = computed(() => {
  const f = found.value;
  if (!f) return "";
  if (!f.artifact) {
    const has = Object.keys(f.manifest.artifacts).join(", ");
    return `None for ${PLATFORM_NAMES[f.target] ?? f.target} — it has ${has}`;
  }
  const [key, a] = f.artifact;
  const size = a.size ? `${formatBytes(a.size)} ` : "";
  const forWhat = key === "any" ? "for every platform" : `for ${PLATFORM_NAMES[key] ?? key}`;
  return `${size}${forWhat}`;
});

const host = computed(() => {
  const url = found.value?.url ?? "";
  const m = /^https:\/\/github\.com\/([^/]+\/[^/]+)\//.exec(url);
  return m ? `github.com/${m[1]}` : url.replace(/^https:\/\//, "").replace(/\/[^/]*$/, "");
});

/// Gallery entries, each marked when this library already has it.
const gallery = computed(() => {
  const have = new Set((packagesState.value?.packages ?? []).map((p) => p.source));
  return GALLERY.map((g) => ({ ...g, added: have.has(g.source) }));
});

function review(g: GalleryEntry) {
  source.value = g.source;
  void look();
}

async function look() {
  if (!source.value.trim()) return;
  looking.value = true;
  error.value = null;
  found.value = null;
  try {
    found.value = await lookupPackage(source.value.trim());
    // The box sits under the whole picker; the review is what to read now.
    await nextTick();
    reviewEl.value?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    looking.value = false;
  }
}

async function add() {
  if (!found.value) return;
  adding.value = true;
  error.value = null;
  try {
    await props.commit(source.value.trim(), found.value);
    emit("done", found.value);
    if (!props.current) {
      found.value = null;
      source.value = "";
    }
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    adding.value = false;
  }
}

function cancel() {
  found.value = null;
  error.value = null;
  emit("cancel");
}

onMounted(() => {
  if (props.current) void look();
});
</script>

<template>
  <section class="pkr">
    <template v-if="!found && !props.current">
      <div v-if="gallery.length" class="pkr-gallery">
        <span class="pkr-label">From the datalib gallery</span>
        <ul class="pkr-list">
          <li v-for="g in gallery" :key="g.source" class="pkr-item">
            <span class="pkr-item-text">
              <b>{{ g.name }}</b>
              <small>{{ g.description }}</small>
              <small
                ><code>{{ g.source }}</code></small
              >
            </span>
            <span v-if="g.added" class="pkr-help">In this library</span>
            <button v-else class="btn" :disabled="looking" @click="review(g)">Review</button>
          </li>
        </ul>
      </div>
      <label class="pkr-label" for="pkr-link">{{
        props.label ?? "Add a package from a link"
      }}</label>
      <div class="pkr-row">
        <input
          id="pkr-link"
          v-model="source"
          class="pkr-input"
          placeholder="github:owner/repo, or a link to datalib-package.toml"
          spellcheck="false"
          @keydown.enter.prevent="look"
        />
        <button class="btn primary" :disabled="looking || !source.trim()" @click="look">
          {{ looking ? "Looking…" : "Look it up" }}
        </button>
      </div>
      <small class="pkr-help">
        A GitHub repo (<code>github:owner/repo</code>) or the link to a
        <code>datalib-package.toml</code>. You'll see what it adds before anything is added.
      </small>
    </template>
    <p v-else-if="!found && looking" class="pkr-help">Looking up {{ source }}…</p>

    <div v-if="found" ref="reviewEl" class="pkr-review">
      <h3 class="pkr-title">
        <template v-if="unchanged">{{ props.current!.name }} is up to date</template>
        <template v-else-if="props.current">
          Update {{ props.current.name }} from {{ props.current.version }} to
          {{ found.manifest.version }}?
        </template>
        <template v-else>Add {{ found.manifest.name }} to this library?</template>
      </h3>
      <p v-if="found.manifest.description" class="pkr-desc">{{ found.manifest.description }}</p>
      <dl v-if="!unchanged" class="pkr-facts">
        <dt>Version</dt>
        <dd>{{ found.manifest.version }}</dd>
        <dt>From</dt>
        <dd>
          <a v-if="found.manifest.homepage" :href="found.manifest.homepage" target="_blank">{{
            host
          }}</a>
          <template v-else>{{ host }}</template>
        </dd>
        <dt>Adds</dt>
        <dd>
          <template v-if="adds.length">
            <div v-for="a in adds" :key="a.name">
              {{ a.what }}, <b>{{ a.name }}</b
              >, {{ a.where }}
            </div>
          </template>
          <template v-else>Nothing it declares</template>
        </dd>
        <dt>Runs</dt>
        <dd>
          <template v-for="(p, i) in programs" :key="p"
            >{{ i ? ", " : "" }}<code>{{ p }}</code></template
          ><template v-if="!programs.length">Nothing yet</template>
        </dd>
        <dt>Download</dt>
        <dd>{{ buildLine }}</dd>
      </dl>
      <p v-if="!unchanged" class="pkr-warn">
        <b>A package runs as you.</b> Its programs can read everything in this library and anything
        else your account can. Add it only if you trust whoever publishes <b>{{ host }}</b
        >.
      </p>
      <details v-if="!unchanged" class="pkr-details">
        <summary>Details</summary>
        <p>
          manifest <code>{{ found.url }}</code
          ><br />sha256 <code>{{ found.sha256 }}</code
          ><br />pinned in config.toml, so this exact build is what runs until you update.
        </p>
      </details>
      <div class="pkr-actions">
        <button class="btn ghost" :disabled="adding" @click="cancel">
          {{ unchanged ? "Close" : "Cancel" }}
        </button>
        <button
          v-if="!unchanged"
          class="btn primary"
          :disabled="adding || !found.artifact"
          @click="add"
        >
          {{
            adding
              ? props.current
                ? "Updating…"
                : "Adding…"
              : props.current
                ? "Update"
                : `Add ${found.manifest.name}`
          }}
        </button>
      </div>
    </div>
    <p v-if="error" class="pkr-error" role="alert">{{ error }}</p>
  </section>
</template>

<style scoped src="./dialog.css"></style>
<style scoped>
.pkr {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.pkr-label {
  font-weight: 600;
}
.pkr-gallery {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.pkr-list {
  list-style: none;
  margin: 0;
  padding: 0;
  border: 1px solid var(--datalib-border-soft);
  border-radius: calc(var(--datalib-radius) + 2px);
}
.pkr-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
}
.pkr-item + .pkr-item {
  border-top: 1px solid var(--datalib-border-soft);
}
.pkr-item-text {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.pkr-item-text small {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.pkr-row {
  display: flex;
  gap: 8px;
}
.pkr-input {
  flex: 1;
  min-width: 0;
  box-sizing: border-box;
  padding: 7px 10px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font: 12px var(--datalib-mono);
}
.pkr-help {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.pkr-review {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.pkr-title {
  margin: 0;
  font-size: var(--datalib-title-size);
}
.pkr-desc {
  margin: 0;
}
.pkr-facts {
  margin: 0;
  display: grid;
  grid-template-columns: 96px minmax(0, 1fr);
  row-gap: 6px;
  column-gap: 12px;
}
.pkr-facts dt {
  color: var(--datalib-muted);
}
.pkr-facts dd {
  margin: 0;
  overflow-wrap: anywhere;
}
.pkr-facts code,
.pkr-details code,
.pkr-item-text code {
  background: var(--datalib-code-bg);
  padding: 1px 4px;
  border-radius: 4px;
  font-family: var(--datalib-mono);
  font-size: 0.92em;
  overflow-wrap: anywhere;
}
.pkr-warn {
  margin: 0;
  background: var(--datalib-warn-bg);
  border: 1px solid var(--datalib-warn-border);
  color: var(--datalib-warn-fg);
  border-radius: var(--datalib-radius);
  padding: 8px 10px;
}
.pkr-details {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.pkr-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.pkr-error {
  margin: 0;
  color: var(--datalib-error-fg);
  background: var(--datalib-error-bg);
  border: 1px solid var(--datalib-error-border);
  border-radius: var(--datalib-radius);
  padding: 8px 10px;
}
</style>
