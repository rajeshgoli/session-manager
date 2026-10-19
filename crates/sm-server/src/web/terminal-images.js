// Capture before xterm's text paste handler; text-only events pass through.
export const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
const IMAGE_TYPES = new Set(['image/png', 'image/jpeg', 'image/gif', 'image/webp']);

function encodedImage(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result).split(',')[1]);
    reader.onerror = () => reject(new Error('Could not read the pasted image'));
    reader.onabort = reader.onerror;
    reader.readAsDataURL(file);
  });
}

export function installImagePaste(host, { connection, upload, paste, report }) {
  let stopped = false, pending = Promise.resolve();
  const handler = (event) => {
    const clipboard = event.clipboardData;
    // Some mobile browsers expose files without DataTransferItem entries.
    const items = Array.from(clipboard?.items || []);
    const images = items.filter(item => item.kind === 'file' && item.type.startsWith('image/'));
    const files = images.length ? images.map(item => item.getAsFile()).filter(Boolean)
      : Array.from(clipboard?.files || []).filter(file => file.type.startsWith('image/'));
    if (!images.length && !files.length) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    const attached = connection();
    if (!attached) { report('Terminal is not connected'); return; }
    if (!files.length) { report('Could not read the pasted image'); return; }
    // Serialize image pastes so consecutive uploads retain their paste order.
    pending = pending.then(async () => {
      for (const file of files) {
        if (stopped || connection() !== attached) return;
        try {
          if (!IMAGE_TYPES.has(file.type)) throw new Error('Paste a PNG, JPEG, GIF, or WebP image');
          if (!file.size || file.size > MAX_IMAGE_BYTES) throw new Error('Image must be between 1 byte and 10 MiB');
          const data = await encodedImage(file);
          if (stopped || connection() !== attached) return;
          const { path } = await upload({ mime: file.type, data });
          // Never paste a late response into a replacement terminal connection.
          if (!stopped && connection() === attached) paste(path);
        } catch (error) {
          if (!stopped) report(`Image paste failed: ${error.message}`);
        }
      }
    });
  };
  host.addEventListener('paste', handler, true);
  return () => { stopped = true; host.removeEventListener('paste', handler, true); };
}
