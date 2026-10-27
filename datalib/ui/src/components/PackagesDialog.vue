<script setup lang="ts">
// The packages this library lists in its config: what each is, which
// sources and applets use it, an update to review, and a box to add one.
// The config is the record, so a copy of the library brings its packages
// along. docs/dev/packages.md.
import { computed, onMounted, ref } from "vue";
import type { PackageLookup, PackageView } from "@/api";
import PackageReview from "@/components/PackageReview.vue";
import { packagesState, refreshPackages } from "@/config/packages";

const props = defineProps<{
  addPackage: (source: string, lookup: PackageLookup) => Promise<void>;
  updatePackage: (id: string, lookup: PackageLookup) => Promise<void>;
  /// Throws with the reason when the package cannot go yet.
  removePackage: (id: string) => Promise<void>;
  retry: (p: PackageView) => Promise<void>;
}>();

const emit = defineEmits<{ (e: "close"): void }>();

const packages = computed(() => packagesState.value?.packages ?? []);
const reviewing = ref<string | null>(null);
const rowError = ref<Record<string, string>>({});
const busy = ref<string | null>(null);

onMounted(() => void refreshPackages());

function nameOf(p: PackageView): string {
  return p.manifest?.name ?? p.id;
}

function from(p: PackageView): string {
  const m = /^https:\/\/github\.com\/([^/]+\/[^/]+)\//.exec(p.url);
  if (m) return `github.com/${m[1]}`;
  return p.source ?? p.url;
}

function stateLine(p: PackageView): string {
  if (p.state === "fetching") return "Fetching…";
  if (p.state === "failed") return `Couldn't fetch it: ${p.error ?? "unknown error"}`;
  return p.used_by.length ? `used by ${p.used_by.join(", ")}` : "nothing uses it yet";
}

async function remove(p: PackageView) {
  busy.value = p.id;
  rowError.value = { ...rowError.value, [p.id]: "" };
  try {
    await props.removePackage(p.id);
    await refreshPackages();
  } catch (e) {
    rowError.value = { ...rowError.value, [p.id]: (e as Error).message };
  } finally {
    busy.value = null;
  }
}

async function retry(p: PackageView) {
  busy.value = p.id;
  try {
    await props.retry(p);
  } catch (e) {
    rowError.value = { ...rowError.value, [p.id]: (e as Error).message };
  } finally {
    busy.value = null;
    await refreshPackages();
  }
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") emit("close");
}
</script>

<template>
  <div class="dialog-backdrop" @click.self="emit('close')" @keydown="onKeydown">
    <div class="pkd dialog" role="dialog" aria-modal="true" aria-label="Packages in this library">
      <header class="dialog-head">
        <h2>Packages in this library</h2>
        <button class="dialog-x" aria-label="Close" @click="emit('close')">×</button>
      </header>
      <div class="dialog-body pkd-body">
        <p class="pkd-intro">
          Programs this library runs besides datalib's own. They are listed in its config.toml, so a
          copy of the library brings them along.
        </p>
        <ul v-if="packages.length" class="pkd-list">
          <li v-for="p in packages" :key="p.id" class="pkd-row">
            <div class="pkd-main">
              <span class="pkd-text">
                <b
                  >{{ nameOf(p) }}
                  <span v-if="p.manifest" class="pkd-version">{{ p.manifest.version }}</span></b
                >
                <small :class="{ 'pkd-bad': p.state === 'failed' }"
                  >{{ from(p) }} · {{ stateLine(p) }}</small
                >
              </span>
              <button
                v-if="p.state === 'failed'"
                class="btn"
                :disabled="busy === p.id"
                @click="retry(p)"
              >
                Try again
              </button>
              <button
                v-if="p.source && p.state === 'installed'"
                class="btn"
                :disabled="busy === p.id"
                @click="reviewing = reviewing === p.id ? null : p.id"
              >
                Check for update
              </button>
              <button class="btn" :disabled="busy === p.id" @click="remove(p)">Remove</button>
            </div>
            <p v-if="rowError[p.id]" class="pkd-error" role="alert">{{ rowError[p.id] }}</p>
            <PackageReview
              v-if="reviewing === p.id && p.source"
              class="pkd-review"
              :current="{
                source: p.source,
                sha256: p.sha256,
                name: nameOf(p),
                version: p.manifest?.version ?? '',
              }"
              :commit="(_s, lookup) => props.updatePackage(p.id, lookup)"
              @done="reviewing = null"
              @cancel="reviewing = null"
            />
          </li>
        </ul>
        <p v-else class="pkd-empty">This library has no packages.</p>
        <PackageReview :commit="props.addPackage" label="Add a package" />
      </div>
    </div>
  </div>
</template>

<style scoped src="./dialog.css"></style>
<style scoped>
.pkd {
  width: min(640px, 100%);
}
.pkd-body {
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.pkd-intro,
.pkd-empty {
  margin: 0;
  color: var(--datalib-muted);
}
.pkd-list {
  list-style: none;
  margin: 0;
  padding: 0;
  border: 1px solid var(--datalib-border-soft);
  border-radius: calc(var(--datalib-radius) + 2px);
}
.pkd-row {
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.pkd-row + .pkd-row {
  border-top: 1px solid var(--datalib-border-soft);
}
.pkd-main {
  display: flex;
  align-items: center;
  gap: 8px;
}
.pkd-text {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.pkd-text small {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
  overflow-wrap: anywhere;
}
.pkd-text small.pkd-bad {
  color: var(--datalib-error-fg);
}
.pkd-version {
  font-weight: 400;
  color: var(--datalib-muted);
}
.pkd-error {
  margin: 0;
  color: var(--datalib-error-fg);
}
.pkd-review {
  padding: 10px 12px;
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface-2);
}
</style>
