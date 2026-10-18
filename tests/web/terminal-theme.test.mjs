// Run: node --test tests/web/terminal-theme.test.mjs
// #2304: the web terminal follows Settings › Appearance; light mirrors the 256-colour palette.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DARK, isLight, lightTheme, mirror, paletteRgb, terminalTheme } from '../../crates/sm-server/src/web/terminal-theme.js';

const doc = (theme) => ({ documentElement: { dataset: theme ? { theme } : {} } });
const media = (dark) => () => ({ matches: dark });
const luma = (hex) => {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
const contrast = (a, b) => { const [x, y] = [luma(a), luma(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };

test('theme choice follows the setting, and the OS on System', () => {
  assert.equal(isLight(doc('light'), media(true)), true);
  assert.equal(isLight(doc('dark'), media(false)), false);
  assert.equal(isLight(doc('system'), media(false)), true);
  assert.equal(isLight(doc('system'), media(true)), false);
  assert.equal(isLight(doc(), media(false)), true);
  assert.equal(terminalTheme(false), DARK);
  assert.equal(terminalTheme(true).background, '#FCFCFA');
});

test('palette indexes match xterm', () => {
  assert.deepEqual(paletteRgb(16), [0, 0, 0]);
  assert.deepEqual(paletteRgb(231), [255, 255, 255]);
  assert.deepEqual(paletteRgb(196), [255, 0, 0]);
  assert.deepEqual(paletteRgb(237), [58, 58, 58]);
  assert.deepEqual(paletteRgb(255), [238, 238, 238]);
});

test('light palette covers 16-255 and keeps Claude\'s colours readable', () => {
  const light = lightTheme();
  assert.equal(light.extendedAnsi.length, 240);
  const at = (i) => light.extendedAnsi[i - 16];
  // White emphasis text and grey dim text on the light background.
  assert.ok(contrast(at(231), light.background) > 12);
  assert.ok(contrast(at(246), light.background) > 4);
  // Text on the prompt box (grey 237) stays readable; the box itself is pale.
  assert.ok(luma(at(237)) > 0.5);
  assert.ok(contrast(light.foreground, at(237)) > 7);
  // Hue survives: a red stays red.
  const red = mirror([255, 0, 0]);
  assert.ok(parseInt(red.slice(1, 3), 16) > parseInt(red.slice(3, 5), 16) * 2);
});
