// Renders the icons procedurally:
//   assets/inxtdrive.ico         app + tray icon: white cloud with an X on a blue tile
//   assets/overlay-synced.ico    Explorer badge: in the cloud
//   assets/overlay-syncing.ico   Explorer badge: uploading / waiting to upload
//   assets/*-256.png             previews
// Usage: node tools/make-icons.mjs
import { writeFileSync, mkdirSync } from 'node:fs';
import { deflateSync } from 'node:zlib';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const BADGE_BLUE = [0, 102, 255];
const GREEN = [22, 163, 74];
const WHITE = [255, 255, 255];

function crc32(buf) {
  let c, crc = 0xffffffff;
  for (let n = 0; n < buf.length; n++) {
    c = (crc ^ buf[n]) & 0xff;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    crc = (crc >>> 8) ^ c;
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}

function png(size, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; ihdr[9] = 6;
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    rgba.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

function ico(images) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(images.length, 4);
  let offset = 6 + 16 * images.length;
  const entries = images.map(({ size, data }) => {
    const e = Buffer.alloc(16);
    e[0] = size >= 256 ? 0 : size;
    e[1] = size >= 256 ? 0 : size;
    e.writeUInt16LE(1, 4);
    e.writeUInt16LE(32, 6);
    e.writeUInt32LE(data.length, 8);
    e.writeUInt32LE(offset, 12);
    offset += data.length;
    return e;
  });
  return Buffer.concat([header, ...entries, ...images.map((i) => i.data)]);
}

// --- geometry helpers (unit square, y down) ---------------------------------
const inCircle = (x, y, cx, cy, r) => (x - cx) ** 2 + (y - cy) ** 2 <= r * r;

function distToSegment(x, y, ax, ay, bx, by) {
  const dx = bx - ax, dy = by - ay;
  const t = Math.max(0, Math.min(1, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)));
  return Math.hypot(x - (ax + t * dx), y - (ay + t * dy));
}

// The logo lives on a 32-unit grid, the same numbers as assets/inxtdrive.svg, app.html
// and the Android launcher icon: a rounded blue tile, a white three-lobe cloud and an X
// cut into the cloud in the tile's colour.
const TILE_TOP = [61, 139, 255];
const TILE_BOTTOM = [0, 82, 217];

function inRoundedRect(u, v, x0, y0, x1, y1, r) {
  const qx = Math.max(x0 + r - u, 0, u - (x1 - r));
  const qy = Math.max(y0 + r - v, 0, v - (y1 - r));
  return u >= x0 && u <= x1 && v >= y0 && v <= y1 && qx * qx + qy * qy <= r * r;
}

function inCloud(u, v) {
  return (
    inCircle(u, v, 16.63, 15.12, 7.04) ||
    inCircle(u, v, 8.93, 19.3, 4.62) ||
    inCircle(u, v, 23.89, 19.74, 4.18) ||
    (u >= 8.93 && u <= 23.89 && v >= 16 && v <= 23.92)
  );
}

function inX(u, v, sizePx) {
  const w = Math.max(1.045, 22 / sizePx);
  return distToSegment(u, v, 13.22, 14.35, 19.6, 20.73) <= w ||
         distToSegment(u, v, 19.6, 14.35, 13.22, 20.73) <= w;
}

function tileColour(u, v) {
  const t = Math.min(1, Math.max(0, (u + v - 2) / 60));
  return TILE_TOP.map((c, i) => Math.round(c + (TILE_BOTTOM[i] - c) * t));
}

function renderApp(size) {
  return render(size, (x, y) => {
    const u = x * 32, v = y * 32;
    if (!inRoundedRect(u, v, 1, 1, 31, 31, 7)) return null;
    if (inCloud(u, v) && !inX(u, v, size)) return WHITE;
    return tileColour(u, v);
  });
}

// Badges sit in the bottom-left corner, as Windows' own overlays do.
const BADGE = { cx: 0.27, cy: 0.73, r: 0.25 };

function inCheck(x, y, sizePx) {
  const { cx, cy, r } = BADGE;
  const w = Math.max(0.035, 0.75 / sizePx);
  return distToSegment(x, y, cx - r * 0.45, cy + r * 0.02, cx - r * 0.1, cy + r * 0.38) <= w ||
         distToSegment(x, y, cx - r * 0.1, cy + r * 0.38, cx + r * 0.5, cy - r * 0.32) <= w;
}

function inArrows(x, y, sizePx) {
  const { cx, cy, r } = BADGE;
  const ringR = r * 0.5, w = Math.max(0.038, 0.85 / sizePx);
  const d = Math.hypot(x - cx, y - cy);
  let a = Math.atan2(y - cy, x - cx);
  const onRing = Math.abs(d - ringR) <= w;
  // two arcs, each leaving a gap where its arrowhead sits
  const deg = ((a * 180) / Math.PI + 360) % 360;
  const inArc = (from, to) => deg >= from && deg <= to;
  if (onRing && (inArc(40, 140) || inArc(220, 320))) return true;
  const head = (angDeg) => {
    const t = (angDeg * Math.PI) / 180;
    const px = cx + ringR * Math.cos(t), py = cy + ringR * Math.sin(t);
    const tx = -Math.sin(t), ty = Math.cos(t);
    const nx = Math.cos(t), ny = Math.sin(t);
    const s = r * 0.28;
    const p1 = [px + tx * s, py + ty * s];
    const p2 = [px - nx * s * 0.8, py - ny * s * 0.8];
    const p3 = [px + nx * s * 0.8, py + ny * s * 0.8];
    const sign = (p, q, rr) => (p[0] - rr[0]) * (q[1] - rr[1]) - (q[0] - rr[0]) * (p[1] - rr[1]);
    const P = [x, y];
    const d1 = sign(P, p1, p2), d2 = sign(P, p2, p3), d3 = sign(P, p3, p1);
    const neg = d1 < 0 || d2 < 0 || d3 < 0, pos = d1 > 0 || d2 > 0 || d3 > 0;
    return !(neg && pos);
  };
  return head(140) || head(320);
}

function renderBadge(size, fill, glyph) {
  const { cx, cy, r } = BADGE;
  return render(size, (x, y) => {
    const d = Math.hypot(x - cx, y - cy);
    if (d > r) return null;
    if (d > r * 0.86) return WHITE;
    return glyph(x, y, size) ? WHITE : fill;
  });
}

function render(size, shade) {
  const ss = 5;
  const out = Buffer.alloc(size * size * 4);
  for (let py = 0; py < size; py++) {
    for (let px = 0; px < size; px++) {
      let n = 0, acc = [0, 0, 0];
      for (let sy = 0; sy < ss; sy++) {
        for (let sx = 0; sx < ss; sx++) {
          const c = shade((px + (sx + 0.5) / ss) / size, (py + (sy + 0.5) / ss) / size);
          if (c) {
            n++;
            acc = acc.map((v, i) => v + c[i]);
          }
        }
      }
      const o = (py * size + px) * 4;
      if (n) {
        out[o] = Math.round(acc[0] / n);
        out[o + 1] = Math.round(acc[1] / n);
        out[o + 2] = Math.round(acc[2] / n);
        out[o + 3] = Math.round((n / (ss * ss)) * 255);
      }
    }
  }
  return out;
}

const sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
mkdirSync(join(root, 'assets'), { recursive: true });
const write = (name, renderer) => {
  const images = sizes.map((s) => ({ size: s, data: png(s, renderer(s)) }));
  writeFileSync(join(root, 'assets', `${name}.ico`), ico(images));
  writeFileSync(join(root, 'assets', `${name}-256.png`), images[images.length - 1].data);
};
write('inxtdrive', renderApp);
write('overlay-synced', (s) => renderBadge(s, GREEN, inCheck));
write('overlay-syncing', (s) => renderBadge(s, BADGE_BLUE, inArrows));
console.log('icons written to assets/');

// Optional: `--preview <file.png>` writes an enlarged contact sheet for review.
const pi = process.argv.indexOf('--preview');
if (pi > 0) {
  const scale = 6, pad = 12;
  const tiles = [16, 24, 32, 48];
  const docIcon = (s) => render(s, (x, y) => (x > 0.22 && x < 0.8 && y > 0.1 && y < 0.92 ? [236, 239, 243] : null));
  const over = (base, top) => {
    const o = Buffer.from(base);
    for (let i = 0; i < o.length; i += 4) {
      const a = top[i + 3] / 255;
      for (let c = 0; c < 3; c++) o[i + c] = Math.round(top[i + c] * a + o[i + c] * (1 - a));
      o[i + 3] = Math.max(o[i + 3], top[i + 3]);
    }
    return o;
  };
  const rows = [
    { bg: [255, 255, 255], img: (s) => renderApp(s) },
    { bg: [32, 32, 32], img: (s) => renderApp(s) },
    { bg: [255, 255, 255], img: (s) => over(docIcon(s), renderBadge(s, BADGE_BLUE, inArrows)) },
    { bg: [255, 255, 255], img: (s) => over(docIcon(s), renderBadge(s, GREEN, inCheck)) },
  ];
  const colW = tiles.map((s) => s * scale + pad);
  const W = colW.reduce((a, b) => a + b, pad);
  const rowH = 48 * scale + pad;
  const H = rows.length * rowH + pad;
  const sheet = Buffer.alloc(W * H * 4);
  for (let i = 0; i < W * H; i++) { sheet[i * 4] = 128; sheet[i * 4 + 1] = 128; sheet[i * 4 + 2] = 128; sheet[i * 4 + 3] = 255; }
  rows.forEach((row, ri) => {
    let x0 = pad;
    tiles.forEach((s, ti) => {
      const img = row.img(s);
      const y0 = pad + ri * rowH;
      for (let y = 0; y < s * scale; y++) {
        for (let x = 0; x < s * scale; x++) {
          const si = (Math.floor(y / scale) * s + Math.floor(x / scale)) * 4;
          const a = img[si + 3] / 255;
          const di = ((y0 + y) * W + x0 + x) * 4;
          for (let c = 0; c < 3; c++) sheet[di + c] = Math.round(img[si + c] * a + row.bg[c] * (1 - a));
        }
      }
      x0 += colW[ti];
    });
  });
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(W, 0); ihdr.writeUInt32BE(H, 4); ihdr[8] = 8; ihdr[9] = 6;
  const raw = Buffer.alloc((W * 4 + 1) * H);
  for (let y = 0; y < H; y++) sheet.copy(raw, y * (W * 4 + 1) + 1, y * W * 4, (y + 1) * W * 4);
  writeFileSync(process.argv[pi + 1], Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw)), chunk('IEND', Buffer.alloc(0)),
  ]));
  console.log('preview written');
}
