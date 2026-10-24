// Render only the visible rows. Thumbnail work follows the current page rather
// than filling thousands of hidden images before the user can browse.

async function decodeThumbnail(source) {
  if (!source.startsWith("data:image/png;base64,")) {
    throw new Error("The preview did not contain a PNG image.");
  }
  const image = document.createElement("img");
  image.src = source;
  await image.decode();
  if (image.naturalWidth === 0 || image.naturalHeight === 0) {
    throw new Error("The preview image could not be displayed.");
  }
}

export class EffectCatalogue {
  constructor({
    scroll,
    grid,
    empty,
    requestThumbnail,
    requestCachedThumbnails = null,
    select,
    changed,
    failed,
    recovered = () => {},
    validateThumbnail = decodeThumbnail,
  }) {
    this.scroll = scroll;
    this.grid = grid;
    this.empty = empty;
    this.requestThumbnail = requestThumbnail;
    this.requestCachedThumbnails = requestCachedThumbnails;
    this.validateThumbnail = validateThumbnail;
    this.select = select;
    this.changed = changed;
    this.failed = failed;
    this.recovered = recovered;
    this.entries = [];
    this.entryById = new Map();
    this.filtered = [];
    this.cards = new Map();
    this.images = new Map();
    this.errors = new Map();
    this.backgroundHidden = new Map();
    this.query = "";
    this.group = "";
    this.visible = new Set();
    this.cacheChecked = new Set();
    this.cacheWorking = false;
    this.queue = [];
    this.selected = null;
    this.background = "dark";
    this.generation = 0;
    this.frame = 0;
    this.working = false;
    this.paused = false;
    this.scroll.addEventListener("scroll", () => this.schedule());
    this.resizeObserver = new ResizeObserver(() => this.schedule());
    this.resizeObserver.observe(this.scroll);
  }

  replace(entries) {
    this.generation += 1;
    this.entries = entries;
    this.entryById = new Map(entries.map((entry) => [entry.id, entry]));
    this.images.clear();
    this.errors.clear();
    this.backgroundHidden.clear();
    this.queue = [];
    this.cards.clear();
    this.visible.clear();
    this.cacheChecked.clear();
    this.grid.replaceChildren();
    this.selected = null;
    this.filter("", "");
  }

  filter(query, group) {
    const name = query.trim().toLowerCase();
    this.query = query;
    this.group = group;
    this.filtered = this.entries.filter((entry) => {
      if (this.backgroundHidden.has(entry.id)) {
        return false;
      }
      if (group && entry.group !== group) {
        return false;
      }
      return entry.name.toLowerCase().includes(name);
    });
    this.scroll.scrollTop = 0;
    this.changed(this.filtered.length, this.entries.length - this.backgroundHidden.size);
    this.schedule();
  }

  setSelected(id) {
    this.selected = id;
    this.schedule();
  }

  imageKey(id, preset = this.background) {
    return `${preset}:${id}`;
  }

  setBackground(preset) {
    if (preset === this.background) {
      return;
    }
    this.background = preset;
    this.generation += 1;
    // A still that is blank on dark grey may be useful on a light backdrop.
    // Only permanent effect exclusions survive a background change.
    for (const id of new Set([...this.errors.keys(), ...this.backgroundHidden.keys()])) {
      this.recovered(id);
    }
    this.errors.clear();
    this.backgroundHidden.clear();
    this.cacheChecked.clear();
    this.queue = [];
    for (const card of this.cards.values()) {
      delete card.children[0].dataset.source;
      card.children[0].replaceChildren();
    }
    // Keep the filter, scroll position, selection and playback independent
    // from thumbnail image generations. Existing variants remain in the LRU.
    const scrollTop = this.scroll.scrollTop;
    this.filter(this.query, this.group);
    this.scroll.scrollTop = scrollTop;
    this.render();
  }

  exclude(id, error) {
    const entry = this.entryById.get(id);
    const reason = String(error).toLowerCase();
    if (!entry || (!reason.startsWith("unsupported:") && !reason.startsWith("empty:"))) {
      return false;
    }
    if (reason.startsWith("empty:")) {
      // Native checks found output on another background. Hide this card only
      // for the current choice and retry it when the user changes backgrounds.
      this.backgroundHidden.set(id, error);
    } else {
      this.entryById.delete(id);
      this.entries = this.entries.filter((item) => item.id !== id);
      this.backgroundHidden.delete(id);
    }
    this.filtered = this.filtered.filter((item) => item.id !== id);
    this.queue = this.queue.filter((queued) => queued !== id);
    for (const key of this.images.keys()) {
      if (key.endsWith(`:${id}`)) {
        this.images.delete(key);
      }
    }
    this.errors.delete(id);
    const card = this.cards.get(id);
    if (card) {
      card.remove();
      this.cards.delete(id);
    }
    this.failed(id, error, entry.name);
    this.changed(this.filtered.length, this.entries.length - this.backgroundHidden.size);
    this.schedule();
    return true;
  }

  setPaused(paused) {
    this.paused = paused;
    if (!paused) {
      this.pump();
    }
  }

  schedule() {
    if (this.frame) {
      return;
    }
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.render();
    });
  }

  createCard(entry) {
    const card = document.createElement("button");
    card.className = "effect-card";
    card.title = entry.name;
    card.setAttribute("aria-label", `Preview ${entry.name}`);
    const thumbnail = document.createElement("span");
    thumbnail.className = "thumbnail";
    thumbnail.textContent = "Preparing preview…";
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = entry.name;
    const badge = document.createElement("span");
    badge.className = "badge";
    badge.textContent = entry.group;
    card.append(thumbnail, name, badge);
    card.addEventListener("click", () => {
      if (this.errors.has(entry.id)) {
        this.errors.delete(entry.id);
        this.cacheChecked.delete(entry.id);
        thumbnail.textContent = "Preparing preview…";
        this.schedule();
      }
      // Selection must not wait for a background thumbnail. It has its own
      // preparation path and takes priority over queued catalogue work.
      this.select(entry);
    });
    return card;
  }

  render() {
    const columns = Math.max(1, Math.floor(this.scroll.clientWidth / 175));
    const width = this.scroll.clientWidth / columns;
    const rowHeight = 190;
    const firstRow = Math.max(0, Math.floor(this.scroll.scrollTop / rowHeight) - 1);
    const lastRow = Math.ceil((this.scroll.scrollTop + this.scroll.clientHeight) / rowHeight) + 1;
    const first = firstRow * columns;
    const last = Math.min(this.filtered.length, lastRow * columns);
    const viewportFirst = Math.floor(this.scroll.scrollTop / rowHeight) * columns;
    const viewportLast = Math.ceil((this.scroll.scrollTop + this.scroll.clientHeight) / rowHeight) * columns;
    const visibleIds = new Set(this.filtered.slice(first, last).map((entry) => entry.id));
    this.visible = new Set(this.filtered.slice(viewportFirst, viewportLast).map((entry) => entry.id));
    this.grid.style.height = `${Math.ceil(this.filtered.length / columns) * rowHeight}px`;
    this.empty.hidden = this.filtered.length > 0;

    for (const [id, card] of this.cards) {
      if (!visibleIds.has(id)) {
        card.remove();
        this.cards.delete(id);
      }
    }

    this.queue = [];
    for (let index = first; index < last; index += 1) {
      const entry = this.filtered[index];
      let card = this.cards.get(entry.id);
      if (!card) {
        card = this.createCard(entry);
        this.cards.set(entry.id, card);
      }
      card.classList.toggle("selected", this.selected === entry.id);
      card.style.left = `${(index % columns) * width + 5}px`;
      card.style.top = `${Math.floor(index / columns) * rowHeight + 5}px`;
      card.style.width = `${width - 10}px`;
      const source = this.images.get(this.imageKey(entry.id));
      card.disabled = false;
      if (source) {
        const thumbnail = card.children[0];
        card.title = entry.name;
        if (thumbnail.dataset.source !== source) {
          const image = document.createElement("img");
          image.src = source;
          image.alt = "";
          thumbnail.replaceChildren(image);
          thumbnail.dataset.source = source;
        }
      } else if (this.errors.has(entry.id)) {
        card.children[0].textContent = "Preview unavailable — click to retry";
        card.title = `${entry.name}\n${this.errors.get(entry.id)}`;
      } else {
        card.children[0].textContent = "Preparing preview…";
        card.title = entry.name;
        this.queue.push(entry.id);
      }
      const position = index - first;
      if (this.grid.children[position] !== card) {
        this.grid.insertBefore(card, this.grid.children[position] || null);
      }
    }
    // Prepare the rows on screen before the extra rows retained for smooth
    // scrolling. A slow off-screen effect must not hold up the visible page.
    this.queue.sort((left, right) => Number(this.visible.has(right)) - Number(this.visible.has(left)));
    this.warmCache();
    this.pump();
  }

  async warmCache() {
    if (!this.requestCachedThumbnails || this.cacheWorking || this.paused) {
      return;
    }
    const ids = this.queue.filter((id) => !this.cacheChecked.has(id)).slice(0, 64);
    if (ids.length === 0) {
      return;
    }
    const generation = this.generation;
    const preset = this.background;
    this.cacheWorking = true;
    for (const id of ids) {
      this.cacheChecked.add(id);
    }
    try {
      // This read-only request bypasses native effect preparation. A slow
      // uncached effect cannot hold already-rendered thumbnails behind it.
      const cached = await this.requestCachedThumbnails(ids, preset);
      for (const id of ids) {
        if (generation !== this.generation) {
          break;
        }
        if (!this.entryById.has(id) || this.backgroundHidden.has(id)) {
          continue;
        }
        const source = cached?.[id];
        if (!source) {
          continue;
        }
        try {
          await this.validateThumbnail(source);
          if (generation !== this.generation) {
            break;
          }
          if (!this.entryById.has(id) || this.backgroundHidden.has(id)) {
            continue;
          }
          this.remember(id, source);
          this.errors.delete(id);
          this.recovered(id);
        } catch {
          // A bad cached PNG is retried through the normal thumbnail request,
          // which can report a named error rather than leaving a blank card.
        }
      }
    } catch {
      // Cache lookup is an optimisation. The normal request still performs
      // the authoritative read/render and reports any actual preview failure.
    } finally {
      this.cacheWorking = false;
      this.render();
    }
  }

  remember(id, source) {
    const key = this.imageKey(id);
    this.images.delete(key);
    this.images.set(key, source);
    // Never evict a visible card: doing so would cause repeated disk reads.
    const limit = Math.max(192, this.cards.size);
    for (const cachedKey of this.images.keys()) {
      if (this.images.size <= limit) {
        break;
      }
      const visible = [...this.cards.keys()].some((id) => this.imageKey(id) === cachedKey);
      if (!visible) {
        this.images.delete(cachedKey);
      }
    }
  }

  async pump() {
    if (this.working || this.paused || this.cacheWorking) {
      return;
    }
    this.working = true;
    try {
      while (this.queue.length > 0 && !this.paused) {
        const id = this.queue.shift();
        if (this.images.has(this.imageKey(id))) {
          continue;
        }
        if (this.requestCachedThumbnails && !this.cacheChecked.has(id)) {
          this.queue.unshift(id);
          this.warmCache();
          break;
        }
        const generation = this.generation;
        const preset = this.background;
        const name = this.entryById.get(id)?.name || id;
        try {
          const result = await this.requestThumbnail(id, preset);
          if (generation !== this.generation || !this.entryById.has(id)) {
            continue;
          }
          const source = typeof result === "string" ? result : result?.png;
          if (!source) {
            throw new Error("The effect did not produce a preview image.");
          }
          // An IPC response is not proof that the browser can show its image.
          // Keep the card in its loading state until decoding has succeeded.
          await this.validateThumbnail(source);
          if (generation !== this.generation || !this.entryById.has(id)) {
            continue;
          }
          this.remember(id, source);
          this.errors.delete(id);
          this.recovered(id);
        } catch (error) {
          if (generation !== this.generation || !this.entryById.has(id)) {
            continue;
          }
          const reason = String(error);
          const interrupted = reason.toLowerCase().startsWith("transient:") &&
            /selected preview takes priority|newer selection replaced/i.test(reason);
          if (interrupted) {
            // Choosing an effect intentionally cancels background thumbnail
            // work. It must not leave a permanent failed card behind.
            this.render();
            await new Promise(requestAnimationFrame);
            continue;
          }
          if (/^(unsupported|empty):/i.test(reason)) {
            this.exclude(id, error);
          } else {
            this.errors.set(id, reason);
            const card = this.cards.get(id);
            if (card) {
              card.children[0].textContent = "Preview unavailable — click to retry";
              card.title = `${card.title}\n${error}`;
            }
            this.failed(id, error, name);
          }
        }
        this.render();
        // Give scrolling and selection a turn between native thumbnail jobs.
        await new Promise(requestAnimationFrame);
      }
    } finally {
      this.working = false;
      if (this.queue.length > 0 && !this.paused && !this.cacheWorking) {
        this.pump();
      }
    }
  }
}
