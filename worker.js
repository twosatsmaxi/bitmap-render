'use strict';

// ── HCL → RGB ──────────────────────────────────────────────────────────────
function hclToRgb(hDeg, c, l) {
  const h = hDeg * Math.PI / 180;
  const a = Math.cos(h) * c, b = Math.sin(h) * c;
  const fy = (l + 16) / 116, fx = a / 500 + fy, fz = fy - b / 200;
  const e = 0.008856, k = 903.3;
  const X = (fx*fx*fx > e ? fx*fx*fx : (116*fx-16)/k) * 0.95047;
  const Y = l > k*e ? Math.pow((l+16)/116, 3) : l/k;
  const Z = (fz*fz*fz > e ? fz*fz*fz : (116*fz-16)/k) * 1.08883;
  const lin = v => v <= 0.0031308 ? 12.92*v : 1.055*Math.pow(v, 1/2.4) - 0.055;
  return [
    Math.max(0, Math.min(255, Math.round(lin(X*3.2406 + Y*-1.5372 + Z*-0.4986) * 255))),
    Math.max(0, Math.min(255, Math.round(lin(X*-0.9689 + Y*1.8758 + Z*0.0415) * 255))),
    Math.max(0, Math.min(255, Math.round(lin(X*0.0557 + Y*-0.2040 + Z*1.0570) * 255)))
  ];
}
const C = 78.225;
const ORANGE = { h: 0.181, l: 0.472 };
const TX_COLOR = (() => {
  const [r, g, b] = hclToRgb(ORANGE.h * 360, C, ORANGE.l * 150);
  return `rgb(${r},${g},${b})`;
})();

// ── Mondrian layout ─────────────────────────────────────────────────────────
class MondrianLayout {
  constructor(width) {
    this.width = width;
    this.rowOffset = 0;
    this.rows = [];
    this.txMap = [];
    this.txs = [];
  }

  _getRow(y) { return this.rows[y - this.rowOffset]; }
  _getSlot(x, y) { const r = this._getRow(y); return r ? r.map[x] : undefined; }

  _addRow() {
    const y = this.rows.length + this.rowOffset;
    const row = { y, slots: [], map: {}, max: 0 };
    this.rows.push(row);
    return row;
  }

  _addSlot(slot) {
    if (slot.r <= 0) return;
    const existing = this._getSlot(slot.x, slot.y);
    if (existing) {
      if (slot.r > existing.r) existing.r = slot.r;
      return existing;
    }
    const row = this._getRow(slot.y);
    if (!row) return;
    let insertAt = null;
    for (let i = 0; i < row.slots.length; i++) {
      if (row.slots[i].x > slot.x) { insertAt = i; break; }
    }
    if (insertAt === null) row.slots.push(slot);
    else row.slots.splice(insertAt, 0, slot);
    row.map[slot.x] = slot;
    return slot;
  }

  _removeSlot(slot) {
    const row = this._getRow(slot.y);
    if (row) { delete row.map[slot.x]; const i = row.slots.indexOf(slot); if (i >= 0) row.slots.splice(i, 1); }
  }

  _fillSlot(slot, sw) {
    const sq = { x: slot.x, y: slot.y, r: sw };
    this._removeSlot(slot);

    for (let ri = slot.y; ri < slot.y + sw; ri++) {
      let row = this._getRow(ri);
      if (row) {
        const collisions = [];
        let maxExcess = 0;
        for (const ts of [...row.slots]) {
          if (!((ts.x + ts.r) <= sq.x || ts.x >= (sq.x + sw))) {
            collisions.push(ts);
            maxExcess = Math.max(maxExcess, Math.max(0, (ts.x + ts.r) - (slot.x + slot.r)));
          }
        }
        if (sq.x + sw < this.width && !row.map[sq.x + sw]) {
          this._addSlot({ x: sq.x + sw, y: ri, r: slot.r - sw + maxExcess });
        }
        for (const col of collisions) {
          col.r = slot.x - col.x;
          if (col.r > 0) ; else this._removeSlot(col);
        }
      } else {
        this._addRow();
        if (slot.x > 0) this._addSlot({ x: 0, y: ri, r: slot.x });
        if (sq.x + sw < this.width) this._addSlot({ x: sq.x + sw, y: ri, r: this.width - (sq.x + sw) });
      }
    }

    for (let ri = Math.max(0, slot.y - sw); ri < slot.y; ri++) {
      const row = this._getRow(ri);
      if (!row) continue;
      for (const ts of [...row.slots]) {
        if (ts.x < sq.x + sw && ts.x + ts.r > sq.x && ts.y + ts.r >= slot.y) {
          const oldW = ts.r;
          ts.r = slot.y - ts.y;
          if (ts.r <= 0) { this._removeSlot(ts); continue; }
          let rem = { x: ts.x + ts.r, y: ts.y, w: oldW - ts.r, h: ts.r };
          while (rem.w > 0 && rem.h > 0) {
            if (rem.w <= rem.h) {
              this._addSlot({ x: rem.x, y: rem.y, r: rem.w });
              rem.y += rem.w; rem.h -= rem.w;
            } else {
              this._addSlot({ x: rem.x, y: rem.y, r: rem.h });
              rem.x += rem.h; rem.w -= rem.h;
            }
          }
        }
      }
    }

    return sq;
  }

  place(tx, size) {
    for (const row of this.rows) {
      for (const slot of [...row.slots]) {
        if (slot.r >= size) {
          tx.square = this._fillSlot(slot, size);
          this._recordTx(tx);
          return tx.square;
        }
      }
    }
    const newRow = this._addRow();
    const slot = { x: 0, y: newRow.y, r: this.width };
    newRow.slots.push(slot); newRow.map[0] = slot;
    tx.square = this._fillSlot(slot, size);
    this._recordTx(tx);
    return tx.square;
  }

  _recordTx(tx) {
    const sq = tx.square;
    for (let dx = 0; dx < sq.r; dx++) {
      for (let dy = 0; dy < sq.r; dy++) {
        const oy = sq.y + dy - this.rowOffset;
        while (this.txMap.length <= oy) this.txMap.push(new Array(this.width).fill(null));
        this.txMap[oy][sq.x + dx] = tx;
      }
    }
  }

  get usedHeight() {
    let max = 0;
    for (const tx of this.txs) {
      if (tx.square) max = Math.max(max, tx.square.y + tx.square.r);
    }
    return max;
  }
}

// ── Binary protocol ─────────────────────────────────────────────────────────
function parseBinaryTxs(buffer) {
  const bytes = new Uint8Array(buffer);
  return Array.from(bytes, (size, index) => ({ index, size: size || 1 }));
}

function layoutBlock(txs) {
  txs.forEach(tx => { tx._size = tx.size; });
  let weight = 0;
  txs.forEach(tx => weight += tx._size * tx._size);
  const width = Math.ceil(Math.sqrt(weight));
  const layout = new MondrianLayout(width);
  txs.forEach(tx => layout.place(tx, tx._size));
  layout.txs = txs;
  return { layout, width };
}

// ── Canvas rendering ────────────────────────────────────────────────────────
function renderSquares(ctx, squares, layoutWidth, usedH, canvasSize) {
  ctx.fillStyle = '#0d1117';
  ctx.fillRect(0, 0, canvasSize, canvasSize);
  const draw = Math.max(layoutWidth, usedH);
  const gridSize = canvasSize / draw;
  const offsetY = (canvasSize - usedH * gridSize) / 2;
  const unitPadding = gridSize / 4;
  ctx.fillStyle = TX_COLOR;
  for (const sq of squares) {
    const px = sq.x * gridSize + unitPadding;
    const py = sq.y * gridSize + offsetY + unitPadding;
    const pw = sq.r * gridSize - unitPadding * 2;
    if (pw <= 0) continue;
    ctx.fillRect(px, py, pw, pw);
  }
}

// ── Worker state & WASM Init ────────────────────────────────────────────────
let _squares = null;
let _layoutWidth = 0;
let _usedHeight = 0;
let _offscreen = null;
let _ctx = null;

let wasmInitPromise = null;
try {
  importScripts('./wasm/pkg/wasm.js');
  wasmInitPromise = wasm_bindgen({ module_or_path: './wasm/pkg/wasm_bg.wasm' })
    .then(() => true)
    .catch((e) => {
      console.error('[Worker] WASM init failed', e);
      return false;
    });
} catch (e) {
  console.error('[Worker] Failed to load WASM script', e);
  wasmInitPromise = Promise.resolve(false);
}

self.onmessage = async function(e) {
  const { type } = e.data;

  if (type === 'layout') {
    const { buffer, canvasSize, offscreenCanvas } = e.data;

    if (offscreenCanvas) {
      _offscreen = offscreenCanvas;
      _ctx = _offscreen.getContext('2d');
    }

    const wasmReady = await wasmInitPromise;
    const bytes = new Uint8Array(buffer);
    
    let layoutWidth = 0;
    let usedHeight = 0;
    let squares = [];

    if (wasmReady) {
      const start = performance.now();
      const results = wasm_bindgen.layout_block(bytes);
      const end = performance.now();
      console.log(`[Worker] layoutBlock WASM took ${(end - start).toFixed(2)}ms for ${bytes.length} txs`);
      
      layoutWidth = results[0];
      usedHeight = results[1];
      for (let i = 0; i < bytes.length; i++) {
        const idx = 2 + i * 3;
        squares.push({
          index: i,
          x: results[idx],
          y: results[idx + 1],
          r: results[idx + 2]
        });
      }
    } else {
      const txs = parseBinaryTxs(buffer);
      const start = performance.now();
      const { layout, width } = layoutBlock(txs);
      const end = performance.now();
      console.log(`[Worker] layoutBlock JS took ${(end - start).toFixed(2)}ms for ${txs.length} txs`);

      layoutWidth = width;
      usedHeight = layout.usedHeight;
      squares = txs
        .filter(tx => tx.square)
        .map(tx => ({ x: tx.square.x, y: tx.square.y, r: tx.square.r, index: tx.index }));
    }

    _layoutWidth = layoutWidth;
    _usedHeight = usedHeight;
    _squares = squares;

    if (_offscreen) {
      _offscreen.width = canvasSize;
      _offscreen.height = canvasSize;
      renderSquares(_ctx, _squares, _layoutWidth, _usedHeight, canvasSize);
    }

    self.postMessage({
      type: 'done',
      txCount: bytes.length,
      squares: _squares,
      layoutWidth: _layoutWidth,
      usedHeight: _usedHeight,
    });

  } else if (type === 'resize') {
    if (!_offscreen || !_squares) return;
    const { canvasSize } = e.data;
    _offscreen.width = canvasSize;
    _offscreen.height = canvasSize;
    renderSquares(_ctx, _squares, _layoutWidth, _usedHeight, canvasSize);
  }
};
