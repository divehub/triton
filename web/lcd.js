// LCD canvas: draws RGBA frames from the worker and scales them crisply (nearest neighbour, optionally by whole
// device-pixel multiples so every emulated pixel has the same size).

const INTEGER_FILL = 0.85;

export class LcdView {
  /**
   * @param {{container: HTMLElement, canvas: HTMLCanvasElement, placeholder: HTMLElement, sizeLabel: HTMLElement}} parts
   */
  constructor({ container, canvas, placeholder, sizeLabel }) {
    this.container = container;
    this.canvas = canvas;
    this.placeholder = placeholder;
    this.sizeLabel = sizeLabel;
    this.context = canvas.getContext('2d', { alpha: false });
    this.integer = true;
    this.frames = 0;
    this.lastDraw = null;
    this.visible = false;
    if (typeof ResizeObserver !== 'undefined') new ResizeObserver(() => this.layout()).observe(container);
    window.addEventListener('resize', () => this.layout());
  }

  setIntegerScaling(enabled) {
    this.integer = !!enabled;
    this.container.classList.toggle('integer', this.integer);
    this.layout();
  }

  /** Draws a frame message `{width, height, buffer}`; returns the buffer for recycling. */
  draw(frame) {
    const { width, height, buffer } = frame;
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
      this.sizeLabel.textContent = `${width} × ${height}`;
      this.layout();
    }
    this.context.putImageData(new ImageData(new Uint8ClampedArray(buffer), width, height), 0, 0);
    this.frames += 1;
    this.lastDraw = Date.now();
    return buffer;
  }

  /** Shows the canvas (a frame is available and the panel is on) or the placeholder text. */
  setVisible(visible, text) {
    this.visible = visible;
    this.canvas.hidden = !visible;
    this.placeholder.hidden = visible;
    if (!visible && text) this.placeholder.textContent = text;
    if (visible) this.layout();
  }

  layout() {
    if (!this.integer) {
      this.canvas.style.width = '';
      this.canvas.style.height = '';
      return;
    }
    const bounds = this.container.getBoundingClientRect();
    if (bounds.width < 1 || bounds.height < 1) return;
    const ratio = window.devicePixelRatio || 1;
    const fit = Math.min((bounds.width * ratio) / this.canvas.width, (bounds.height * ratio) / this.canvas.height);
    const scale = Math.floor(fit);
    // Whole device-pixel multiples when they use at least 85% of the available size; otherwise (a phone, a small
    // window) fit the container with nearest-neighbour scaling rather than showing a much smaller picture.
    if (scale < 1 || scale < fit * INTEGER_FILL) {
      this.canvas.style.width = '100%';
      this.canvas.style.height = '100%';
      return;
    }
    this.canvas.style.width = `${(this.canvas.width * scale) / ratio}px`;
    this.canvas.style.height = `${(this.canvas.height * scale) / ratio}px`;
  }
}
