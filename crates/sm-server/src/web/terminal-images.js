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

export function installImagePaste(host, { connection, upload, paste, pasteText = paste, report }) {
  let stopped = false, pending = Promise.resolve();
  const current = (attached) => !stopped && connection() === attached;
  const pasteFiles = async (files, attached) => {
    for (const file of files) {
      if (!current(attached)) return;
      try {
        if (!IMAGE_TYPES.has(file.type)) throw new Error('Paste a PNG, JPEG, GIF, or WebP image');
        if (!file.size || file.size > MAX_IMAGE_BYTES) throw new Error('Image must be between 1 byte and 10 MiB');
        const data = await encodedImage(file);
        if (!current(attached)) return;
        const { path } = await upload({ mime: file.type, data });
        if (current(attached)) paste(path);
      } catch (error) {
        if (!stopped) report(`Image paste failed: ${error.message}`);
      }
    }
  };
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
    pending = pending.then(() => pasteFiles(files, attached));
  };
  // macOS emits a native paste event for Cmd+V, but Control+V is a terminal
  // key. Read the browser clipboard during that key's user gesture; never ask
  // the remote agent to read its host's clipboard. Other OSes keep native paste.
  const keydown = (event) => {
    if (!/Mac|iPhone|iPad/.test(globalThis.navigator?.platform || '')
      || !event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || event.key.toLowerCase() !== 'v') return;
    event.preventDefault();
    event.stopImmediatePropagation();
    if (event.repeat) return;
    const attached = connection();
    if (!attached) { report('Terminal is not connected'); return; }
    const failed = () => { if (current(attached)) report('Could not read the browser clipboard. Allow clipboard access, or paste with Cmd+V.'); };
    let reading;
    try {
      // Start the read now, rather than after queued uploads, while browser
      // user activation is still available. Handle rejection immediately too.
      reading = navigator.clipboard.read().then(items => ({ items }), error => ({ error }));
    } catch { failed(); return; }
    pending = pending.then(async () => {
      try {
        const { items, error } = await reading;
        if (!current(attached)) return;
        if (error) throw error;
        const images = items.map(item => ({ item, type: item.types.find(type => IMAGE_TYPES.has(type)) || item.types.find(type => type.startsWith('image/')) }))
          .filter(({ type }) => type);
        if (images.length) {
          const files = await Promise.all(images.map(({ item, type }) => item.getType(type)));
          await pasteFiles(files, attached);
        } else {
          const text = items.find(item => item.types.includes('text/plain'));
          if (!text) { report('Clipboard has no image or text to paste'); return; }
          const value = await (await text.getType('text/plain')).text();
          if (current(attached)) pasteText(value);
        }
      } catch { failed(); }
    });
  };
  host.addEventListener('paste', handler, true);
  host.addEventListener('keydown', keydown, true);
  return () => {
    stopped = true;
    host.removeEventListener('paste', handler, true);
    host.removeEventListener('keydown', keydown, true);
  };
}
