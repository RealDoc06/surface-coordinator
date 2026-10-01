// Draws the application icon (a hub with four connected devices) as a
// 1024x1024 PNG, without any dependencies. The platform icon files in
// crates/manager/icons are generated from it:
//
//   node scripts/make-icon.mjs
//   npx @tauri-apps/cli@2 icon crates/manager/icons/source.png -o crates/manager/icons
import fs from 'node:fs';
import zlib from 'node:zlib';

const SIZE = 1024;
const pixels = Buffer.alloc(SIZE * SIZE * 4);

const clamp = (v, lo = 0, hi = 1) => Math.min(hi, Math.max(lo, v));
const mix = (a, b, t) => a + (b - a) * t;

// Signed distances (negative inside).
const roundedBox = (x, y, half, radius) => {
  const qx = Math.abs(x) - half + radius;
  const qy = Math.abs(y) - half + radius;
  return Math.hypot(Math.max(qx, 0), Math.max(qy, 0)) + Math.min(Math.max(qx, qy), 0) - radius;
};
const circle = (x, y, cx, cy, r) => Math.hypot(x - cx, y - cy) - r;
const segment = (x, y, ax, ay, bx, by, width) => {
  const t = clamp(((x - ax) * (bx - ax) + (y - ay) * (by - ay)) / ((bx - ax) ** 2 + (by - ay) ** 2));
  return Math.hypot(x - (ax + (bx - ax) * t), y - (ay + (by - ay) * t)) - width;
};
// Coverage of a shape at a pixel from its distance, one pixel of smoothing.
const cover = (distance) => clamp(0.5 - distance);

const centre = SIZE / 2;
const satellites = [[-1, -1], [1, -1], [-1, 1], [1, 1]].map(([sx, sy]) => [centre + sx * 250, centre + sy * 250]);

for (let y = 0; y < SIZE; y++) {
  for (let x = 0; x < SIZE; x++) {
    const px = x + 0.5;
    const py = y + 0.5;
    const background = cover(roundedBox(px - centre, py - centre, 464, 210));
    const shade = y / SIZE;
    let r = mix(46, 22, shade);
    let g = mix(122, 78, shade);
    let b = mix(240, 190, shade);

    let white = 0;
    for (const [sx, sy] of satellites) {
      white = Math.max(white, cover(segment(px, py, centre, centre, sx, sy, 22)));
      white = Math.max(white, cover(circle(px, py, sx, sy, 92)));
    }
    white = Math.max(white, cover(circle(px, py, centre, centre, 150)));
    // The hub is a ring: punch its middle back to the background colour.
    const hole = cover(circle(px, py, centre, centre, 84));
    white = white * (1 - hole);

    r = mix(r, 255, white);
    g = mix(g, 255, white);
    b = mix(b, 255, white);
    const i = (y * SIZE + x) * 4;
    pixels[i] = Math.round(r);
    pixels[i + 1] = Math.round(g);
    pixels[i + 2] = Math.round(b);
    pixels[i + 3] = Math.round(background * 255);
  }
}

const crcTable = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
const crc32 = (buffer) => {
  let c = 0xffffffff;
  for (const byte of buffer) c = crcTable[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
};
const chunk = (type, data) => {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
};

const header = Buffer.alloc(13);
header.writeUInt32BE(SIZE, 0);
header.writeUInt32BE(SIZE, 4);
header[8] = 8; // bit depth
header[9] = 6; // RGBA
const raw = Buffer.alloc((SIZE * 4 + 1) * SIZE);
for (let y = 0; y < SIZE; y++) {
  pixels.copy(raw, y * (SIZE * 4 + 1) + 1, y * SIZE * 4, (y + 1) * SIZE * 4);
}
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk('IHDR', header),
  chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
  chunk('IEND', Buffer.alloc(0)),
]);
const out = new URL('../crates/manager/icons/source.png', import.meta.url);
fs.writeFileSync(out, png);
console.log(`wrote ${out.pathname} (${png.length} bytes)`);
