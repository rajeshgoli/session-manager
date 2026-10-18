// Terminal themes (#2304): each has a light and a dark variant, picked by Settings › Appearance.
// Agents draw mostly with the 256-colour palette (through tmux). Claude's theme is "auto": it asks
// the terminal for its background (xterm answers with the theme's) and draws a light or a dark
// palette to suit, so a colour keeps its lightness here. Each variant only tints the palette:
// greys run from the theme's darker end to its lighter end, and hues take the theme's nearest
// accent, darkened or lightened to the original's lightness. Text still drawn for the other
// appearance (until the agent asks again) is lifted by CONTRAST_FLOOR.

const ANSI = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'];
const variant = (background, foreground, normal, bright, extra = {}) => ({
  background, foreground, cursor: foreground, cursorAccent: background,
  ...Object.fromEntries(ANSI.map((name, i) => [name, normal[i]])),
  ...Object.fromEntries(ANSI.map((name, i) => [`bright${name[0].toUpperCase()}${name.slice(1)}`, bright[i]])),
  ...extra,
});

export const THEMES = {
  sm: {
    label: 'sm',
    light: variant('#FBFBF9', '#24262D',
      ['#2E3038', '#C8283F', '#18804A', '#A86A0B', '#2B62D9', '#9C36B5', '#0E7C91', '#8B8D96'],
      ['#6B6E79', '#E0405A', '#22985A', '#C17F12', '#4479EB', '#B44FCD', '#1795AB', '#B9BBC3'],
      { selectionBackground: '#0B7A9030' }),
    dark: variant('#0E0F14', '#D7DAE3',
      ['#2A2C36', '#F2667C', '#4FCB82', '#E8B54E', '#6FA1FF', '#CC8AF0', '#4FD3E6', '#C3C7D3'],
      ['#5C6070', '#FF8698', '#72E09F', '#F7CE73', '#93BAFF', '#DDA8F8', '#7CE3F2', '#F2F4F8'],
      { selectionBackground: '#5EE7FF33' }),
  },
  github: {
    label: 'GitHub',
    light: variant('#FFFFFF', '#1F2328',
      ['#24292F', '#CF222E', '#116329', '#4D2D00', '#0969DA', '#8250DF', '#1B7C83', '#6E7781'],
      ['#57606A', '#A40E26', '#1A7F37', '#633C01', '#218BFF', '#A475F9', '#3192AA', '#8C959F'],
      { selectionBackground: '#0969DA2E' }),
    dark: variant('#0D1117', '#E6EDF3',
      ['#484F58', '#FF7B72', '#3FB950', '#D29922', '#58A6FF', '#BC8CFF', '#39C5CF', '#B1BAC4'],
      ['#6E7681', '#FFA198', '#56D364', '#E3B341', '#79C0FF', '#D2A8FF', '#56D4DD', '#FFFFFF'],
      { selectionBackground: '#388BFD40' }),
  },
  solarized: {
    label: 'Solarized',
    light: variant('#FDF6E3', '#586E75',
      ['#073642', '#DC322F', '#859900', '#B58900', '#268BD2', '#D33682', '#2AA198', '#EEE8D5'],
      ['#002B36', '#CB4B16', '#586E75', '#657B83', '#839496', '#6C71C4', '#93A1A1', '#FDF6E3'],
      { selectionBackground: '#EEE8D5' }),
    dark: variant('#002B36', '#93A1A1',
      ['#073642', '#DC322F', '#859900', '#B58900', '#268BD2', '#D33682', '#2AA198', '#EEE8D5'],
      ['#002B36', '#CB4B16', '#586E75', '#657B83', '#839496', '#6C71C4', '#93A1A1', '#FDF6E3'],
      { selectionBackground: '#073642' }),
  },
  catppuccin: {
    label: 'Catppuccin',
    light: variant('#EFF1F5', '#4C4F69',
      ['#5C5F77', '#D20F39', '#40A02B', '#DF8E1D', '#1E66F5', '#EA76CB', '#179299', '#ACB0BE'],
      ['#6C6F85', '#D20F39', '#40A02B', '#DF8E1D', '#1E66F5', '#EA76CB', '#179299', '#BCC0CC'],
      { selectionBackground: '#7C7F9340' }),
    dark: variant('#1E1E2E', '#CDD6F4',
      ['#45475A', '#F38BA8', '#A6E3A1', '#F9E2AF', '#89B4FA', '#F5C2E7', '#94E2D5', '#BAC2DE'],
      ['#585B70', '#F38BA8', '#A6E3A1', '#F9E2AF', '#89B4FA', '#F5C2E7', '#94E2D5', '#A6ADC8'],
      { selectionBackground: '#9399B240' }),
  },
};
export const DEFAULT_THEME = 'sm';
export const THEME_KEY = 'sm-term-theme';

// sRGB <-> OKLab (Björn Ottosson's matrices).
const toLinear = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const toGamma = (c) => (c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055);
const parse = (hex) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
function oklab([r, g, b]) {
  [r, g, b] = [r, g, b].map((v) => toLinear(v / 255));
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  return [0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s];
}
function toHex([L, a, b]) {
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  const rgb = [4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707612701 * s];
  return '#' + rgb.map((v) => Math.round(Math.min(1, Math.max(0, toGamma(v))) * 255).toString(16).padStart(2, '0')).join('').toUpperCase();
}
const mix = (p, q, t) => p.map((v, i) => v + (q[i] - v) * t);

/** xterm's colour for palette index 16-255: the 6x6x6 cube, then the 24-step grey ramp. */
export function paletteRgb(index) {
  if (index >= 232) { const v = 8 + (index - 232) * 10; return [v, v, v]; }
  const i = index - 16, step = (n) => (n ? 55 + n * 40 : 0);
  return [step(Math.floor(i / 36)), step(Math.floor(i / 6) % 6), step(i % 6)];
}

/** One palette colour, re-coloured into a theme variant. */
export function recolour(rgb, v) {
  const [L, a, b] = oklab(rgb);
  const bg = oklab(parse(v.background)), fg = oklab(parse(v.foreground));
  const [dark, bright] = bg[0] < fg[0] ? [bg, fg] : [fg, bg];
  const grey = () => toHex(mix(dark, bright, Math.min(1, Math.max(0, L))));
  if (Math.hypot(a, b) < 0.03) return grey();
  // A hue takes the theme's nearest accent at the original's lightness: a dark original (a diff
  // background on a dark screen) fades toward the dark end, a pale one toward the light end.
  const hue = Math.atan2(b, a);
  let best = null, gap = Infinity;
  for (const name of ANSI.slice(1, 7)) {
    const c = oklab(parse(v[name]));
    const d = Math.abs(Math.atan2(Math.sin(Math.atan2(c[2], c[1]) - hue), Math.cos(Math.atan2(c[2], c[1]) - hue)));
    if (Math.hypot(c[1], c[2]) > 0.03 && d < gap) { gap = d; best = c; }
  }
  if (!best) return grey();
  if (L <= best[0]) return toHex(mix(dark, best, (L / best[0]) ** 2));
  return toHex(mix(best, bright, Math.min(1, (L - best[0]) / (1 - best[0]))));
}

const cache = new Map();
/** The xterm theme for a theme name and appearance; unknown names fall back to sm. */
export function terminalTheme(light, name = DEFAULT_THEME) {
  const theme = THEMES[name] || THEMES[DEFAULT_THEME];
  const key = `${THEMES[name] ? name : DEFAULT_THEME}:${light ? 'light' : 'dark'}`;
  if (!cache.has(key)) {
    const v = light ? theme.light : theme.dark;
    const extendedAnsi = [];
    for (let i = 16; i < 256; i++) extendedAnsi.push(recolour(paletteRgb(i), v));
    cache.set(key, { ...v, extendedAnsi });
  }
  return cache.get(key);
}

/** Light when Settings says light, or says system and the OS is light. */
export function isLight(doc = document, media = window.matchMedia) {
  const theme = doc.documentElement.dataset.theme || 'system';
  return theme === 'light' || (theme === 'system' && !media('(prefers-color-scheme: dark)').matches);
}

/** Text on a background of nearly its own colour (SGR 40 on light, say) is lifted to this contrast. */
export const CONTRAST_FLOOR = 4.5;
