// The top bar's bug button (1859 B1, B2): capture the page as it is, then
// open the Start dialog in its Report-a-bug mode.
import { useState } from 'preact/hooks';
import { html, pageData } from './ui.js';
import { TicketStart } from './board-start.js';

export const SCREENSHOT_MAX_BYTES = 8 * 1024 * 1024;

export const decodedBytes = (b64) => Math.floor((b64.length * 3) / 4) - (b64.endsWith('==') ? 2 : b64.endsWith('=') ? 1 : 0);

/**
 * The page as a base64 PNG: at the screen's ratio (at most 2), then once at
 * ratio 1 after a failure or an image over 8 MiB; null when both fail.
 */
export async function captureScreen({ node, ratio, render } = {}) {
  const attempt = async (pixelRatio) => {
    const draw = render || (await import('html-to-image')).toPng;
    const url = String(await draw(node || document.body, { pixelRatio, filter: (n) => n.dataset?.bugExclude === undefined }));
    const b64 = url.slice(url.indexOf(',') + 1);
    if (!url.startsWith('data:image/png') || decodedBytes(b64) > SCREENSHOT_MAX_BYTES) throw new Error('screenshot unusable');
    return b64;
  };
  try {
    return await attempt(Math.min(ratio || window.devicePixelRatio || 1, 2));
  } catch (e) {
    try { return await attempt(1); } catch (again) { return null; }
  }
}

/** `page` is the current page's nav label. */
export function BugButton({ page }) {
  const [report, setReport] = useState(null);
  const capturing = report === 'capturing';
  const press = async () => {
    if (report) return;
    // What the page had loaded, before the dialog's own fetches.
    const data = pageData.forPage();
    const route = location.pathname + location.search;
    setReport('capturing');
    const screenshot = await captureScreen();
    setReport({ screenshot, page_data: data, page, route });
  };
  return html`<button type="button" class="icon-btn bug-btn" title="Report a bug" aria-label="Report a bug" disabled=${capturing} onClick=${press}>🐞</button>
    ${report && !capturing ? html`<div class="board-start-overlay" data-bug-exclude="1">
      <${TicketStart} mode="bug" bug=${report} onClose=${() => setReport(null)} /></div>` : null}`;
}
