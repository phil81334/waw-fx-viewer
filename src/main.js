import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { EffectCatalogue } from "./catalogue.js";
import { NativeViewport } from "./viewport.js";

const element = (id) => document.getElementById(id);
const desktop = isTauri();
const backgroundPresets = new Set(["dark", "grey", "light", "checkerboard"]);
let background = "dark";
let committedBackground = "dark";
let backgroundRevision = 0;
let backgroundPending = Promise.resolve();
let thumbnailBackground = "dark";
let committedThumbnailBackground = "dark";
let thumbnailBackgroundRevision = 0;
let thumbnailBackgroundPending = Promise.resolve();
let selected = null;
let catalogueStatistics = null;
let scanBusy = false;
let cacheBusy = false;
let selectionGeneration = 0;
let selectedRevision = null;
let selectionBusy = false;
let loaded = false;
let playing = false;
let duration = 0;
let durationKnown = false;
let continuous = false;
let currentTime = 0;
const previewFailures = new Map();
let modalOpening = false;
let lightReceiver = false;
const eventUnlisteners = [];

function message(text, error = false, details = text) {
  element("message").textContent = text;
  element("message").className = error ? "error" : "success";
  if (error) {
    reportError(details, "ui-error");
  }
}

function effectError(name, error) {
  let detail = String(error);
  while (/^(unsupported|transient|empty):\s*/i.test(detail)) {
    detail = detail.replace(/^(unsupported|transient|empty):\s*/i, "");
  }
  const canonical = (text) => text.replaceAll("\\", "/").toLowerCase();
  if (canonical(detail).startsWith(`${canonical(name)}:`)) {
    detail = detail.slice(name.length + 1).trimStart();
  }
  return `${name}: ${detail}`;
}

function reportError(error, kind) {
  if (desktop) {
    // Diagnostic recording must never replace the useful error with a second
    // error if the log location is unavailable.
    invoke("report_ui", { message: String(error), kind }).catch(() => {});
  }
}

window.addEventListener("error", (event) => {
  reportError(event.error?.stack || event.message || "Unknown browser error", "uncaught");
});
window.addEventListener("unhandledrejection", (event) => {
  reportError(event.reason?.stack || event.reason || "Unknown rejected promise", "rejection");
});

const viewport = new NativeViewport(element("viewport"), invoke, (error) => {
  message(`Could not update the preview: ${error}`, true);
});

const catalogue = new EffectCatalogue({
  scroll: element("grid-scroll"),
  grid: element("grid"),
  empty: element("empty-grid"),
  requestThumbnail: (id, preset) => invoke("thumbnail", { id, preset }),
  requestCachedThumbnails: (ids, preset) => invoke("cached_thumbnails", { ids, preset }),
  select: selectEffect,
  changed: (shown, total) => {
    element("count").textContent = `${shown.toLocaleString()} of ${total.toLocaleString()} effects`;
    updateStatistics();
  },
  failed: (id, error, name) => {
    const reason = String(error);
    previewFailures.set(id, {
      name,
      reason,
      excluded: /^(unsupported|empty):/i.test(reason),
    });
    updateStatistics();
    if (selected?.id === id && /^(unsupported|empty):/i.test(reason)) {
      resetPreview();
      catalogue.setPaused(scanBusy || cacheBusy);
      viewport.setReady(false).catch((failure) => message(String(failure), true));
      element("status").textContent = "Done";
      message("This effect has been left out because it could not produce a usable preview.");
    }
    // Background failures belong to their named catalogue entries. They must
    // not overwrite an unrelated selected effect's playback/error message.
    reportError(`Thumbnail ${name}: ${reason}`, "thumbnail");
  },
  recovered: (id) => {
    if (previewFailures.delete(id)) {
      updateStatistics();
    }
  },
});

function applyBackground(preset) {
  background = preset;
  element("background").value = preset;
  element("viewport").dataset.background = preset;
}

function applyThumbnailBackground(preset) {
  thumbnailBackground = preset;
  element("thumbnail-background").value = preset;
  element("grid-scroll").dataset.background = preset;
  catalogue.setBackground(preset);
}

async function changeBackground(preset) {
  if (!backgroundPresets.has(preset)) {
    element("background").value = background;
    return;
  }
  const revision = ++backgroundRevision;
  // Preserve viewport command order without touching thumbnail generations.
  backgroundPending = backgroundPending.catch(() => {}).then(async () => {
    if (revision !== backgroundRevision) {
      return;
    }
    try {
      if (desktop) {
        await invoke("set_background", { preset });
      }
      committedBackground = preset;
      if (revision === backgroundRevision) {
        applyBackground(preset);
      }
    } catch (error) {
      if (revision === backgroundRevision) {
        applyBackground(committedBackground);
        message(`Could not change the viewport background: ${error}`, true);
      }
    }
  });
  await backgroundPending;
}

async function changeThumbnailBackground(preset) {
  if (!backgroundPresets.has(preset)) {
    element("thumbnail-background").value = thumbnailBackground;
    return;
  }
  const revision = ++thumbnailBackgroundRevision;
  // This queue is independent of viewport colour and playback. A stale result
  // cannot replace a newer thumbnail choice or its variant-cache generation.
  thumbnailBackgroundPending = thumbnailBackgroundPending.catch(() => {}).then(async () => {
    if (revision !== thumbnailBackgroundRevision) {
      return;
    }
    try {
      if (desktop) {
        await invoke("set_thumbnail_background", { preset });
      }
      committedThumbnailBackground = preset;
      if (revision === thumbnailBackgroundRevision) {
        applyThumbnailBackground(preset);
      }
    } catch (error) {
      if (revision === thumbnailBackgroundRevision) {
        applyThumbnailBackground(committedThumbnailBackground);
        message(`Could not change the thumbnail background: ${error}`, true);
      }
    }
  });
  await thumbnailBackgroundPending;
}

function updateStatistics() {
  if (!catalogueStatistics) {
    return;
  }
  const found = Number(catalogueStatistics.found || 0);
  const failedPreviews = [...previewFailures.values()].filter((failure) => failure.excluded).length;
  const supported = Math.max(0, Number(catalogueStatistics.supported || 0) - failedPreviews);
  const excluded = Number(catalogueStatistics.excluded || 0) + failedPreviews;
  const retryable = previewFailures.size - failedPreviews;
  const previewIssues = retryable > 0 ? ` · ${retryable.toLocaleString()} previews unavailable` : "";
  element("statistics-summary").textContent = `${found.toLocaleString()} effects found · ${supported.toLocaleString()} available to preview · ${excluded.toLocaleString()} left out${previewIssues}`;
  element("statistics").textContent = "The catalogue checks effect definitions and required materials. Thumbnails are rendered as you browse; this is not a promise that every effect has already been tested end to end. Left out does not necessarily mean broken. Sound elements are ignored.";
  const errors = [...(catalogueStatistics.errors || []), ...previewFailures.values()];
  element("catalogue-errors").hidden = errors.length === 0;
  element("catalogue-errors").textContent = errors.map((item) => {
    if (typeof item === "string") {
      return item;
    }
    const reason = item.message || item.reason || JSON.stringify(item);
    if (item.name) {
      return effectError(item.name, reason);
    }
    return reason;
  }).join("\n");
}

function updatePlayback() {
  for (const id of ["play", "restart", "speed", "loop", "reset-view", "plane"]) {
    element(id).disabled = !loaded || selectionBusy;
  }
  element("play").textContent = playing ? "Pause" : "Play";
  if (continuous) {
    element("playback-time").textContent = `${currentTime.toFixed(2)} s · continuous`;
  } else if (!durationKnown) {
    element("playback-time").textContent = `${currentTime.toFixed(2)} s`;
  } else {
    element("playback-time").textContent = `${currentTime.toFixed(2)} / ${duration.toFixed(2)} s`;
  }
}

async function control(action, value = null) {
  if (!desktop || !loaded || selectionBusy) {
    return;
  }
  const revision = selectedRevision;
  try {
    await invoke("playback_control", { selection: revision, action, value });
  } catch (error) {
    if (revision === selectedRevision) {
      message(`Could not change playback: ${error}`, true);
    }
  }
}

async function selectEffect(entry) {
  if (!desktop || scanBusy || cacheBusy) {
    return;
  }
  const generation = ++selectionGeneration;
  selected = entry;
  selectedRevision = null;
  loaded = false;
  playing = false;
  selectionBusy = true;
  lightReceiver = false;
  element("plane-control").hidden = true;
  currentTime = 0;
  duration = 0;
  durationKnown = false;
  continuous = false;
  catalogue.setSelected(entry.id);
  catalogue.setPaused(true);
  updatePlayback();
  element("filename").textContent = entry.name;
  element("copy-name").disabled = false;
  element("effect-info").textContent = "Preparing native preview frames…";
  element("preview-empty").hidden = false;
  element("preview-empty").textContent = "Preparing preview…";
  element("status").textContent = "Loading…";
  message("Preparing the selected effect. You can keep browsing or choose another effect.");
  try {
    await viewport.setReady(false);
    if (generation !== selectionGeneration) {
      return;
    }
    const result = await invoke("select_effect", { id: entry.id });
    if (generation !== selectionGeneration) {
      return;
    }
    selectedRevision = result.selection;
    continuous = result.continuous === true;
    durationKnown = Number.isFinite(result.duration);
    duration = Number(result.duration || 0);
    lightReceiver = result.lightReceiver === true;
    element("plane-control").hidden = !lightReceiver;
    loaded = true;
    playing = true;
    selectionBusy = false;
    const description = ["Visual preview"];
    if (continuous) {
      description.push("continuous effect");
    }
    if (result.focusedView === true) {
      description.push("close-up · shows part of the effect");
    }
    if (result.delayedActivity === true) {
      description.push("authored starting delay");
    }
    element("effect-info").textContent = description.join(" · ");
    element("preview-empty").hidden = true;
    await viewport.setReady(true);
    const settings = [
      ["speed", Number(element("speed").value)],
      ["loop", element("loop").checked],
    ];
    if (lightReceiver) {
      settings.push(["plane", element("plane").checked]);
    }
    for (const [action, value] of settings) {
      if (generation !== selectionGeneration) {
        return;
      }
      await control(action, value);
    }
    // The final settings command may finish after another card was selected.
    // Its completion must not replace that selection's loading/error message.
    if (generation !== selectionGeneration) {
      return;
    }
    element("status").textContent = "Done";
    message("Effect loaded. Drag to pan, wheel to zoom.");
    updatePlayback();
  } catch (error) {
    if (generation !== selectionGeneration) {
      return;
    }
    loaded = false;
    playing = false;
    await viewport.setReady(false).catch(() => {});
    if (generation !== selectionGeneration) {
      return;
    }
    element("preview-empty").hidden = false;
    element("preview-empty").textContent = "This effect could not be previewed. Select another effect or retry.";
    element("effect-info").textContent = "Preview unavailable.";
    element("status").textContent = "Done";
    catalogue.exclude(entry.id, error);
    message(effectError(entry.name, error), true, String(error));
  } finally {
    if (generation === selectionGeneration) {
      selectionBusy = false;
      updatePlayback();
      catalogue.setPaused(false);
    }
  }
}

function applyFilters() {
  catalogue.filter(element("search").value, element("group").value);
  element("empty-grid").textContent = "No effects match these filters.";
}

function resetPreview() {
  selectionGeneration += 1;
  selected = null;
  selectedRevision = null;
  loaded = false;
  playing = false;
  selectionBusy = false;
  lightReceiver = false;
  element("plane-control").hidden = true;
  currentTime = 0;
  duration = 0;
  durationKnown = false;
  continuous = false;
  catalogue.setSelected(null);
  element("filename").textContent = "Effect preview";
  element("copy-name").disabled = true;
  element("preview-empty").hidden = false;
  element("preview-empty").textContent = "Select an effect to preview it.";
  element("effect-info").textContent = "Select a thumbnail to play an effect.";
  updatePlayback();
}

async function scan(force = false) {
  if (!desktop || scanBusy || cacheBusy) {
    return;
  }
  scanBusy = true;
  resetPreview();
  previewFailures.clear();
  catalogue.setPaused(true);
  catalogue.replace([]);
  catalogueStatistics = null;
  element("statistics-summary").textContent = "Catalogue information";
  element("statistics").textContent = "Checking the installed effects…";
  element("catalogue-errors").hidden = true;
  element("empty-grid").textContent = "Reading the Modtools effects…";
  element("rescan").disabled = true;
  element("clear-cache").disabled = true;
  element("status").textContent = "Scanning…";
  message("Reading the installed effect catalogue…");
  updatePlayback();
  try {
    await viewport.setReady(false);
    const result = await invoke("scan_catalogue", { force });
    catalogueStatistics = result;
    catalogue.replace(result.entries);
    const previous = element("group").value;
    element("group").replaceChildren(new Option("All folders", ""));
    const groups = [...new Set(result.entries.map((entry) => entry.group))].sort();
    for (const group of groups) {
      element("group").append(new Option(group, group));
    }
    element("group").value = previous;
    if (element("group").selectedIndex < 0) {
      element("group").value = "";
    }
    applyFilters();
    message(`${result.entries.length.toLocaleString()} effects available to preview. Thumbnails load as you browse.`);
    element("status").textContent = "Ready";
  } catch (error) {
    element("count").textContent = "No catalogue loaded";
    element("empty-grid").textContent = "Could not read effects. See the message below, then click Rescan.";
    element("status").textContent = "Done";
    message(String(error), true);
  } finally {
    scanBusy = false;
    element("rescan").disabled = false;
    element("clear-cache").disabled = false;
    catalogue.setPaused(false);
  }
}

async function openDialog(id) {
  if (modalOpening || document.querySelector("dialog[open]")) {
    return false;
  }
  modalOpening = true;
  try {
    if (desktop) {
      await viewport.setModal(true);
    }
    element(id).showModal();
    if (id === "about-dialog") {
      element("repo-url").focus();
      element("repo-url").select();
    }
    return true;
  } catch (error) {
    message(`Could not open the dialog: ${error}`, true);
    await viewport.setModal(false).catch(() => {});
    return false;
  } finally {
    modalOpening = false;
  }
}

async function openCache() {
  const opened = await openDialog("cache-dialog");
  if (!opened) {
    return;
  }
  element("cache-size").textContent = "Reading cache…";
  try {
    const info = await invoke("cache_info");
    element("cache-path").value = info.path;
    const megabytes = (Number(info.bytes || 0) / (1024 * 1024)).toFixed(1);
    element("cache-size").textContent = `${Number(info.files || 0).toLocaleString()} cached files · ${megabytes} MB`;
  } catch (error) {
    element("cache-size").textContent = `Could not read the cache: ${error}`;
  }
}

function handleStatus(update) {
  if (!selected || selectedRevision === null || update.selection !== selectedRevision) {
    return;
  }
  if (update.id) {
    if (update.id !== selected.id) {
      return;
    }
  } else if (update.name) {
    // The backend canonicalises dependency names, while the catalogue keeps
    // their on-disk spelling for copying. A matching ID is authoritative.
    const canonical = (name) => name.replaceAll("\\", "/").toLowerCase();
    if (canonical(update.name) !== canonical(selected.name)) {
      return;
    }
  }
  if (Number.isFinite(update.time)) {
    currentTime = update.time;
  }
  if (typeof update.continuous === "boolean") {
    continuous = update.continuous;
  }
  if (Number.isFinite(update.duration)) {
    durationKnown = true;
    duration = update.duration;
  }
  if (update.state === "playing") {
    playing = true;
  } else if (["paused", "ended", "stopped", "error"].includes(update.state)) {
    playing = false;
  }
  if (update.state === "loading") {
    loaded = false;
    playing = false;
    viewport.setReady(false).catch((error) => message(String(error), true));
    element("preview-empty").hidden = false;
    element("preview-empty").textContent = "Preparing preview…";
    element("effect-info").textContent = update.message || "Preparing native preview frames…";
  } else if (update.state === "error") {
    loaded = false;
    viewport.setReady(false).catch((error) => message(String(error), true));
    element("preview-empty").hidden = false;
    element("preview-empty").textContent = "Playback stopped. Select another effect or retry.";
    const reason = update.message || "The effect could not be played.";
    catalogue.exclude(selected.id, reason);
    message(effectError(selected.name, reason), true, reason);
    element("status").textContent = "Done";
  }
  updatePlayback();
}

function handleScanProgress(update) {
  // A delayed scan event must never overwrite a selected effect's playback
  // status. The listener is installed once, rather than once per rescan.
  if (!scanBusy || selected) {
    return;
  }
  let text = "Reading the installed effects…";
  if (update.phase === "cache") {
    text = "Reading the saved effect catalogue…";
  } else if (update.phase === "prepare") {
    const completed = Number(update.completed || 0).toLocaleString();
    const total = Number(update.total || 0).toLocaleString();
    text = `Checking effects… ${completed} of ${total}`;
  }
  element("status").textContent = text;
  element("empty-grid").textContent = text;
  message(text);
}

function removeEventListeners() {
  for (const unlisten of eventUnlisteners.splice(0)) {
    unlisten();
  }
}

window.addEventListener("pagehide", removeEventListeners, { once: true });

element("search").addEventListener("input", applyFilters);
element("group").addEventListener("change", applyFilters);
element("background").addEventListener("change", (event) => changeBackground(event.target.value));
element("thumbnail-background").addEventListener("change", (event) => changeThumbnailBackground(event.target.value));
element("reset-filters").addEventListener("click", () => {
  element("search").value = "";
  element("group").value = "";
  applyFilters();
});
element("rescan").addEventListener("click", () => scan(true));
element("play").addEventListener("click", () => control(playing ? "pause" : "play"));
element("restart").addEventListener("click", () => control("restart"));
element("speed").addEventListener("change", () => control("speed", Number(element("speed").value)));
element("loop").addEventListener("change", () => control("loop", element("loop").checked));
element("plane").addEventListener("change", () => control("plane", element("plane").checked));
element("reset-view").addEventListener("click", () => control("reset_view"));
element("about").addEventListener("click", () => openDialog("about-dialog"));
element("cache-button").addEventListener("click", openCache);
element("repo-url").addEventListener("click", (event) => event.target.select());
element("cache-path").addEventListener("click", (event) => event.target.select());

for (const [button, dialog] of [["close-about", "about-dialog"], ["close-cache", "cache-dialog"]]) {
  element(button).addEventListener("click", () => element(dialog).close());
  element(dialog).addEventListener("close", () => {
    viewport.setModal(false).catch((error) => message(String(error), true));
  });
}

element("clear-cache").addEventListener("click", async () => {
  if (scanBusy || cacheBusy) {
    return;
  }
  cacheBusy = true;
  element("clear-cache").disabled = true;
  element("rescan").disabled = true;
  catalogue.setPaused(true);
  // The native clear command stops the selected player even if deleting the
  // cache later fails. Invalidate its UI token before the command starts.
  resetPreview();
  element("status").textContent = "Clearing…";
  message("Clearing the preview cache…");
  try {
    await viewport.setReady(false);
    await invoke("cache_clear");
    element("cache-dialog").close();
    cacheBusy = false;
    await scan(true);
  } catch (error) {
    element("status").textContent = "Done";
    message(`Could not clear the cache: ${error}`, true);
  } finally {
    cacheBusy = false;
    element("clear-cache").disabled = false;
    element("rescan").disabled = scanBusy;
    catalogue.setPaused(scanBusy || selectionBusy);
  }
});

element("copy-name").addEventListener("click", async () => {
  if (!selected) {
    return;
  }
  try {
    await navigator.clipboard.writeText(selected.name);
    message("Effect name copied.");
    element("status").textContent = "Done";
  } catch (error) {
    message(`Could not copy the effect name: ${error}`, true);
  }
});

element("startup-error").addEventListener("cancel", (event) => event.preventDefault());
element("quit").addEventListener("click", () => invoke("quit"));
async function start() {
  let folderValidated = false;
  try {
    if (desktop) {
      const context = await invoke("get_context");
      if (!context.valid || !context.modtoolsPresent) {
        throw new Error(context.error || "Place this EXE beside CoDWaW.exe in a World at War folder with Modtools installed.");
      }
      folderValidated = true;
      const preset = backgroundPresets.has(context.background) ? context.background : "dark";
      committedBackground = preset;
      applyBackground(preset);
      const thumbnailPreset = backgroundPresets.has(context.thumbnailBackground)
        ? context.thumbnailBackground
        : "dark";
      committedThumbnailBackground = thumbnailPreset;
      applyThumbnailBackground(thumbnailPreset);
      element("environment").textContent = context.development ? "Development" : "Production";
      element("app-version").textContent = `WaW FX Viewer | ${context.version}`;
      eventUnlisteners.push(await listen("fx-status", (event) => handleStatus(event.payload)));
      eventUnlisteners.push(await listen("fx-scan-progress", (event) => handleScanProgress(event.payload)));
    } else {
      element("environment").textContent = "Development";
    }
    element("app").hidden = false;
    element("footer").hidden = false;
    if (desktop) {
      await viewport.setEnabled(true);
      await scan();
    } else {
      element("count").textContent = "Browser UI preview";
      element("empty-grid").textContent = "Run the desktop app to browse your installed Modtools effects.";
      element("rescan").disabled = true;
      element("cache-button").disabled = true;
      element("plane").disabled = true;
      element("statistics").textContent = "Installed files and native playback are available in the desktop app.";
      message("Browser UI preview only. Use npm run dev for the desktop app and native playback.");
    }
  } catch (error) {
    removeEventListeners();
    await viewport.setReady(false).catch(() => {});
    element("app").hidden = true;
    element("footer").hidden = true;
    if (folderValidated) {
      element("startup-title").textContent = "Could not open the viewer";
    }
    element("startup-message").textContent = String(error);
    element("startup-error").showModal();
  }
}

start();
