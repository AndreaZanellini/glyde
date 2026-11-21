// Regenerates every app-icon file from packaging/icon/glyde.svg (issue #232).
//
//   node packaging/icon/build_icons.mjs
//
// Needs Node and Playwright with Chromium (`npm i -g playwright && npx
// playwright install chromium`). Chromium draws the SVG at each size, so a
// 16 px icon is the vector drawn at 16 px, not a 1024 px image shrunk. This
// script then writes the containers itself, with no other tool:
//
//   packaging/icon/png/glyde-<size>.png  Linux (AppImage, hicolor) and the
//                                        window icon set by glyde-app
//   packaging/windows/glyde.ico          glyde.exe (embedded by
//                                        crates/glyde-app/build.rs) and the installer
//   packaging/macos/Glyde.icns           the Glyde.app bundle
//
// macOS gets the SVG as drawn (Apple's grid leaves a 100 px inset around the
// tile); Windows and Linux get it cropped to a 2 % margin, as their icons
// fill the canvas.
//
// The output files are committed: building or packaging Glyde never runs this.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..");
const svg = readFileSync(join(here, "glyde.svg"), "utf8");

// The full 1024 grid for macOS; 856 px around the 824 px tile elsewhere.
const fullSvg = svg;
const croppedSvg = svg.replace('viewBox="0 0 1024 1024"', 'viewBox="84 84 856 856"');
if (croppedSvg === svg) throw new Error("glyde.svg: expected viewBox=\"0 0 1024 1024\"");

const browser = await chromium.launch();
const page = await browser.newPage();

/** Draws `source` at size×size; returns { png: Buffer, rgba: Buffer }. */
async function render(source, size) {
  const result = await page.evaluate(
    async ({ source, size }) => {
      const img = new Image(size, size);
      img.src = "data:image/svg+xml;base64," + btoa(unescape(encodeURIComponent(source)));
      await img.decode();
      const canvas = document.createElement("canvas");
      canvas.width = size;
      canvas.height = size;
      const ctx = canvas.getContext("2d");
      ctx.drawImage(img, 0, 0, size, size);
      const rgba = Array.from(ctx.getImageData(0, 0, size, size).data);
      const png = canvas.toDataURL("image/png").split(",")[1];
      return { png, rgba };
    },
    { source, size },
  );
  return { png: Buffer.from(result.png, "base64"), rgba: Buffer.from(result.rgba) };
}

// --- PNGs (Linux, window icon) ----------------------------------------------
const pngSizes = [16, 24, 32, 48, 64, 128, 256, 512];
const cropped = new Map();
mkdirSync(join(here, "png"), { recursive: true });
for (const size of pngSizes) {
  const image = await render(croppedSvg, size);
  cropped.set(size, image);
  writeFileSync(join(here, "png", `glyde-${size}.png`), image.png);
}

// --- .ico (Windows) ----------------------------------------------------------
// Below 256 px, classic 32-bit DIB frames, which every Windows tool reads;
// 256 px as PNG, as Windows itself stores it.
function dibFrame(rgba, size) {
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0); // biSize
  header.writeInt32LE(size, 4); // biWidth
  header.writeInt32LE(size * 2, 8); // biHeight: XOR + AND masks
  header.writeUInt16LE(1, 12); // biPlanes
  header.writeUInt16LE(32, 14); // biBitCount
  const xor = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y++) {
    const src = (size - 1 - y) * size * 4; // DIBs are stored bottom-up
    for (let x = 0; x < size; x++) {
      const s = src + x * 4;
      const d = (y * size + x) * 4;
      xor[d] = rgba[s + 2]; // B
      xor[d + 1] = rgba[s + 1]; // G
      xor[d + 2] = rgba[s]; // R
      xor[d + 3] = rgba[s + 3]; // A
    }
  }
  // AND mask: all zero (the alpha channel decides), rows padded to 32 bits.
  const and = Buffer.alloc(Math.ceil(size / 32) * 4 * size);
  return Buffer.concat([header, xor, and]);
}

const icoSizes = [16, 24, 32, 48, 64, 128, 256];
const icoFrames = icoSizes.map((size) => {
  const { png, rgba } = cropped.get(size);
  return { size, data: size >= 256 ? png : dibFrame(rgba, size) };
});
const icoHeader = Buffer.alloc(6 + 16 * icoFrames.length);
icoHeader.writeUInt16LE(0, 0); // reserved
icoHeader.writeUInt16LE(1, 2); // type: icon
icoHeader.writeUInt16LE(icoFrames.length, 4);
let offset = icoHeader.length;
icoFrames.forEach(({ size, data }, i) => {
  const e = 6 + 16 * i;
  icoHeader.writeUInt8(size >= 256 ? 0 : size, e); // 0 means 256
  icoHeader.writeUInt8(size >= 256 ? 0 : size, e + 1);
  icoHeader.writeUInt8(0, e + 2); // palette colors
  icoHeader.writeUInt8(0, e + 3); // reserved
  icoHeader.writeUInt16LE(1, e + 4); // planes
  icoHeader.writeUInt16LE(32, e + 6); // bits per pixel
  icoHeader.writeUInt32LE(data.length, e + 8);
  icoHeader.writeUInt32LE(offset, e + 12);
  offset += data.length;
});
writeFileSync(
  join(root, "packaging", "windows", "glyde.ico"),
  Buffer.concat([icoHeader, ...icoFrames.map((f) => f.data)]),
);

// --- .icns (macOS) -----------------------------------------------------------
// The PNG element types `iconutil` writes for a full .iconset.
const icnsTypes = [
  ["icp4", 16],
  ["icp5", 32],
  ["ic11", 32], // 16 @2x
  ["ic12", 64], // 32 @2x
  ["ic07", 128],
  ["ic13", 256], // 128 @2x
  ["ic08", 256],
  ["ic14", 512], // 256 @2x
  ["ic09", 512],
  ["ic10", 1024], // 512 @2x
];
const full = new Map();
const icnsElements = [];
for (const [type, size] of icnsTypes) {
  if (!full.has(size)) full.set(size, (await render(fullSvg, size)).png);
  const png = full.get(size);
  const head = Buffer.alloc(8);
  head.write(type, 0, "ascii");
  head.writeUInt32BE(8 + png.length, 4);
  icnsElements.push(head, png);
}
const icnsBody = Buffer.concat(icnsElements);
const icnsHead = Buffer.alloc(8);
icnsHead.write("icns", 0, "ascii");
icnsHead.writeUInt32BE(8 + icnsBody.length, 4);
writeFileSync(join(root, "packaging", "macos", "Glyde.icns"), Buffer.concat([icnsHead, icnsBody]));

await browser.close();
console.log("icons written: packaging/icon/png/, packaging/windows/glyde.ico, packaging/macos/Glyde.icns");
