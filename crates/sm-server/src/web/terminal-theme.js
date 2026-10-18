// Terminal colours that follow Settings › Appearance (#2304).
// Agents paint for a dark screen: Claude draws emphasis in palette white (231), dim text in
// greys (244, 246) and its prompt box on grey 237. A light terminal keeps those indexes but
// gives each of the 256 palette colours its hue at mirrored lightness, so white text turns
// near-black and dark backgrounds turn pale, and every pairing keeps its contrast.

export const DARK = { background: '#0E0F14', foreground: '#D7DAE3' };

const LIGHT_ANSI = {
  black: '#16171B', red: '#BE123C', green: '#15803D', yellow: '#A16207',
  blue: '#1D4ED8', magenta: '#A21CAF', cyan: '#0E7490', white: '#555864',
  brightBlack: '#8B8D96', brightRed: '#E11D48', brightGreen: '#16A34A', brightYellow: '#B45309',
  brightBlue: '#2563EB', brightMagenta: '#C026D3', brightCyan: '#0891B2', brightWhite: '#16171B',
};

// sRGB <-> OKLab (Björn Ottosson's matrices).
const toLinear = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const toGamma = (c) => (c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055);
function oklab([r, g, b]) {
  [r, g, b] = [r, g, b].map((v) => toLinear(v / 255));
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  return [0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s];
}
function linearRgb([L, a, b]) {
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  return [4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707612701 * s];
}
const hex = (rgb) => '#' + rgb.map((v) => Math.round(Math.min(1, Math.max(0, toGamma(v))) * 255).toString(16).padStart(2, '0')).join('');

/** xterm's colour for palette index 16-255: the 6x6x6 cube, then the 24-step grey ramp. */
export function paletteRgb(index) {
  if (index >= 232) { const v = 8 + (index - 232) * 10; return [v, v, v]; }
  const i = index - 16, step = (n) => (n ? 55 + n * 40 : 0);
  return [step(Math.floor(i / 36)), step(Math.floor(i / 6) % 6), step(i % 6)];
}

/** The same hue at mirrored lightness: black becomes near-white, white becomes near-black. */
export function mirror(rgb) {
  const [L, a, b] = oklab(rgb);
  const target = 0.985 - 0.785 * L ** 1.6;
  // Shrink chroma until the colour fits sRGB, so a saturated hue does not clip to a different one.
  for (let k = 1; k > 0; k -= 0.05) {
    const out = linearRgb([target, a * k, b * k]);
    if (out.every((v) => v >= -0.0005 && v <= 1.0005)) return hex(out);
  }
  return hex(linearRgb([target, 0, 0]));
}

let lightCache = null;
export function lightTheme() {
  if (!lightCache) {
    const extendedAnsi = [];
    for (let i = 16; i < 256; i++) extendedAnsi.push(mirror(paletteRgb(i)));
    lightCache = {
      background: '#FCFCFA', foreground: '#16171B', cursor: '#16171B', cursorAccent: '#FCFCFA',
      selectionBackground: '#0B7A9040', ...LIGHT_ANSI, extendedAnsi,
    };
  }
  return lightCache;
}

/** Light when Settings says light, or says system and the OS is light. */
export function isLight(doc = document, media = window.matchMedia) {
  const theme = doc.documentElement.dataset.theme || 'system';
  return theme === 'light' || (theme === 'system' && !media('(prefers-color-scheme: dark)').matches);
}

export const terminalTheme = (light) => (light ? lightTheme() : DARK);
