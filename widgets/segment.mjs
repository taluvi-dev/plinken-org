// Segmented button row — the control for stepped params with 2–4 named
// positions (mode switches, on/off pairs, monitor selectors). Reads far
// better than a pot with a name lookup: every option is visible and the
// active one is lit.
//
//   const seg = new Segment(container, {
//     id: PID_MODE, options: ['WIDE', 'SPLIT'], default: 0, label: 'Mode',
//   });
//   seg.onChange((value, id) => transport.sendSet(id, value));
//
// `value` is the option index as a float, matching a PARAM_IS_STEPPED
// ParamDef with min 0, max options.length-1.

const STYLE_ID = 'plk-segment-styles';

const STYLES = `
.segment { display: flex; flex-direction: column; align-items: center; gap: 4px; }
.segment-row {
  display: flex;
  border: 1px solid var(--border-soft);
  border-radius: 3px;
  overflow: hidden;
  background: var(--bg-deep);
}
.segment-btn {
  font-family: var(--font-display);
  font-size: 0.55rem;
  letter-spacing: 0.1em;
  color: var(--text-dim);
  background: transparent;
  border: none;
  padding: 4px 8px;
  cursor: pointer;
  text-transform: uppercase;
}
.segment-btn + .segment-btn { border-left: 1px solid var(--border-soft); }
.segment-btn[data-active="1"] { color: var(--bg-deep); background: var(--accent); }
.segment-btn:focus-visible { outline: 1px solid var(--accent); outline-offset: -1px; }
.segment-label {
  font-family: var(--font-display);
  font-size: 0.55rem;
  letter-spacing: 0.14em;
  color: var(--text-dim);
  text-transform: uppercase;
}
`;

function injectStyles() {
  if (document.getElementById(STYLE_ID)) return;
  const style = document.createElement('style');
  style.id = STYLE_ID;
  style.textContent = STYLES;
  document.head.appendChild(style);
}

export class Segment {
  /**
   * @param {HTMLElement} container
   * @param {object} cfg
   * @param {number} cfg.id            — CLAP param id
   * @param {string[]} cfg.options     — one label per position
   * @param {number} [cfg.default=0]   — active option index
   * @param {string} [cfg.label]
   */
  constructor(container, cfg) {
    injectStyles();
    this.id = cfg.id;
    this.options = cfg.options;
    this.value = cfg.default ?? 0;
    this.listeners = new Set();

    this.el = document.createElement('div');
    this.el.className = 'segment';
    this.el.dataset.id = String(this.id);

    const row = document.createElement('div');
    row.className = 'segment-row';
    this.buttons = this.options.map((name, i) => {
      const b = document.createElement('button');
      b.className = 'segment-btn';
      b.type = 'button';
      b.textContent = name;
      b.addEventListener('click', () => this.#setFromUI(i));
      row.appendChild(b);
      return b;
    });
    this.el.appendChild(row);

    if (cfg.label) {
      const label = document.createElement('div');
      label.className = 'segment-label';
      label.textContent = cfg.label;
      this.el.appendChild(label);
    }
    container.appendChild(this.el);
    this.#render();
  }

  setValue(v) {
    const next = Math.max(0, Math.min(this.options.length - 1, Math.round(v)));
    if (next === this.value) return;
    this.value = next;
    this.#render();
  }

  setValueFromHost(v) {
    this.setValue(v);
  }

  onChange(cb) {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  }

  #render() {
    this.buttons.forEach((b, i) => {
      b.dataset.active = i === this.value ? '1' : '0';
    });
  }

  #setFromUI(i) {
    if (i === this.value) return;
    this.value = i;
    this.#render();
    for (const cb of this.listeners) cb(this.value, this.id);
  }
}
