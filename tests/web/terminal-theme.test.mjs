// Run: node --test tests/web/terminal-theme.test.mjs
// #2304: terminal themes with light and dark variants, chosen by Settings › Appearance.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { CONTRAST_FLOOR, DEFAULT_THEME, THEMES, isLight, paletteRgb, recolour, terminalTheme } from '../../crates/sm-server/src/web/terminal-theme.js';

const doc = (theme) => ({ documentElement: { dataset: theme ? { theme } : {} } });
const media = (dark) => () => ({ matches: dark });
const luma = (hex) => {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
const contrast = (a, b) => { const [x, y] = [luma(a), luma(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };

test('appearance follows the setting, and the OS on System', () => {
  assert.equal(isLight(doc('light'), media(true)), true);
  assert.equal(isLight(doc('dark'), media(false)), false);
  assert.equal(isLight(doc('system'), media(false)), true);
  assert.equal(isLight(doc('system'), media(true)), false);
  assert.equal(isLight(doc(), media(false)), true);
});

test('every theme has readable light and dark variants; unknown names fall back to sm', () => {
  for (const name of Object.keys(THEMES)) {
    const light = terminalTheme(true, name), dark = terminalTheme(false, name);
    assert.ok(luma(light.background) > luma(light.foreground), name);
    assert.ok(luma(dark.background) < luma(dark.foreground), name);
    assert.ok(contrast(light.background, light.foreground) > 4.5, name);
    assert.ok(contrast(dark.background, dark.foreground) > 4.5, name);
    assert.equal(light.extendedAnsi.length, 240);
  }
  assert.equal(terminalTheme(true, 'nope'), terminalTheme(true, DEFAULT_THEME));
  assert.equal(terminalTheme(false), terminalTheme(false, 'sm'));
  assert.ok(CONTRAST_FLOOR >= 4.5);
});

test('palette indexes match xterm', () => {
  assert.deepEqual(paletteRgb(16), [0, 0, 0]);
  assert.deepEqual(paletteRgb(231), [255, 255, 255]);
  assert.deepEqual(paletteRgb(196), [255, 0, 0]);
  assert.deepEqual(paletteRgb(237), [58, 58, 58]);
  assert.deepEqual(paletteRgb(255), [238, 238, 238]);
});

test('recolouring keeps lightness, so Claude\'s light and dark palettes both read right', () => {
  for (const light of [true, false]) {
    const v = terminalTheme(light, 'sm');
    const at = (i) => v.extendedAnsi[i - 16];
    // Black and white land on the theme's dark and light ends.
    assert.equal(at(16), light ? v.foreground : v.background);
    assert.equal(at(231), light ? v.background : v.foreground);
    // Claude's light palette (fg 16 on box 255) in light, its dark one (231 on 237) in dark.
    if (light) assert.ok(contrast(at(16), at(255)) > 10);
    else assert.ok(contrast(at(231), at(237)) > 5);
  }
  // A hue takes the theme's accent: Claude's green 114 becomes a green.
  const green = recolour(paletteRgb(114), THEMES.solarized.dark);
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(green.slice(i, i + 2), 16));
  assert.ok(g > r && g > b, green);
});
