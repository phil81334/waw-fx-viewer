// The D3D surface is a native child window. Its bounds use physical pixels;
// dialogs must hide it explicitly because HTML cannot paint over that HWND.
export class NativeViewport {
  constructor(element, invoke, onError) {
    this.element = element;
    this.invoke = invoke;
    this.onError = onError;
    this.enabled = false;
    this.ready = false;
    this.modal = false;
    this.revision = 0;
    this.frame = 0;
    this.pending = Promise.resolve();
    this.resizeObserver = new ResizeObserver(() => this.schedule());
    this.resizeObserver.observe(element);
    window.addEventListener("resize", () => this.schedule());
    document.addEventListener("visibilitychange", () => this.schedule());
    document.addEventListener("scroll", () => this.schedule(), true);
  }

  setEnabled(enabled) {
    if (this.enabled && !enabled) {
      this.ready = false;
      return this.sync(true).finally(() => {
        this.enabled = false;
      });
    }
    this.enabled = enabled;
    return this.sync();
  }

  setReady(ready) {
    this.ready = ready;
    return this.sync(!ready);
  }

  setModal(modal) {
    this.modal = modal;
    return this.sync(modal);
  }

  schedule() {
    if (this.frame) {
      return;
    }
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.sync().catch(this.onError);
    });
  }

  sync(mandatory = false) {
    if (!this.enabled) {
      return Promise.resolve();
    }
    const revision = ++this.revision;
    const rectangle = this.element.getBoundingClientRect();
    const scale = window.devicePixelRatio || 1;
    const bounds = {
      x: Math.round((rectangle.left + 1) * scale),
      y: Math.round((rectangle.top + 1) * scale),
      width: Math.max(1, Math.round((rectangle.width - 2) * scale)),
      height: Math.max(1, Math.round((rectangle.height - 2) * scale)),
      scale,
      visible: this.ready && !this.modal && rectangle.width > 2 &&
        rectangle.height > 2 && document.visibilityState !== "hidden",
    };
    // Serialising show/hide ensures a late resize cannot uncover a dialog.
    this.pending = this.pending.catch(() => {}).then(async () => {
      // Hiding for a dialog is a barrier: callers await its completion before
      // opening HTML that the native surface must never cover. A later resize
      // may supersede ordinary layout updates, but must not skip that barrier.
      if (!mandatory && revision !== this.revision) {
        return;
      }
      await this.invoke("viewport_bounds", { bounds });
    });
    return this.pending;
  }
}
