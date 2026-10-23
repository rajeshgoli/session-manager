// Browser regression for Chrome refusing CSP-sandboxed passive documents (#2501).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const source = await readFile(new URL('../crates/sm-server/src/http/terminal_files.rs', import.meta.url), 'utf8');
// Exercise the actual policy strings emitted by the Rust handler; Rust tests
// separately verify the MIME-to-policy choice and owner authentication.
const active = source.match(/"(sandbox; default-src[^"\n]+)"/)[1];
const passive = source.match(/"(default-src[^"\n]+)"/)[1];
// Decode the handler's Rust string literal so Chrome parses its actual wrapper.
const textTemplate = JSON.parse(source.match(/bytes = format!\((".*")\)\.into_bytes\(\);/)[1]);

test('Chrome preserves leading blank lines in terminal text readers', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage();
    for (const text of ['first\nsecond\n', '\nfirst\n', '\r\nfirst\r\n', '\n\nfirst\n', '']) {
      const body = textTemplate.replaceAll('{{', '{').replaceAll('}}', '}')
        .replace('{title}', 'source.txt').replace('{text}', text);
      await page.route('https://sm.example.com/source.txt', route => route.fulfill({
        contentType: 'text/html; charset=utf-8',
        headers: { 'content-security-policy': active, 'x-content-type-options': 'nosniff' },
        body,
      }));
      await page.goto('https://sm.example.com/source.txt');
      assert.equal(await page.locator('pre').textContent(), text.replaceAll('\r\n', '\n'),
        `reader preserves blank lines for ${JSON.stringify(text)}`);
      await page.unroute('https://sm.example.com/source.txt');
    }
  } finally { await browser.close(); }
});

test('Chrome displays text, images and sandboxed HTML previews', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage();
    await page.route('https://sm.example.com/**', route => {
      const name = new URL(route.request().url()).pathname;
      const headers = { 'content-security-policy': name === '/memo.html' ? active : passive, 'x-content-type-options': 'nosniff' };
      if (name === '/source.rs') return route.fulfill({ contentType: 'text/plain; charset=utf-8', headers, body: 'fn main() {}' });
      if (name === '/image.png') return route.fulfill({ contentType: 'image/png', headers, body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6RZsAAAAASUVORK5CYII=', 'base64') });
      return route.fulfill({ contentType: 'text/html; charset=utf-8', headers, body: '<h1>Memo</h1><script>document.body.textContent="script executed"</script>' });
    });
    await page.goto('https://sm.example.com/source.rs');
    assert.equal(await page.locator('body').innerText(), 'fn main() {}');
    await page.goto('https://sm.example.com/image.png');
    assert.equal(await page.locator('img').evaluate(img => img.naturalWidth), 1);
    await page.goto('https://sm.example.com/memo.html');
    assert.equal(await page.locator('h1').innerText(), 'Memo');
    assert.equal(await page.locator('body').innerText(), 'Memo');
  } finally { await browser.close(); }
});
