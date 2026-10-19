// Appearance owns each reader's initial theme; a doc toggle may change this view.
export function followReaderTheme(frame, outer = document, storage = localStorage,
  media = matchMedia('(prefers-color-scheme: dark)'), Observer = MutationObserver) {
  const apply = () => {
    let theme = outer.documentElement.dataset.theme;
    if (!theme) { try { theme = storage.getItem('sm-theme'); } catch (_) {} }
    if (theme !== 'light' && theme !== 'dark') theme = media.matches ? 'dark' : 'light';
    try { frame.contentWindow.document.documentElement.dataset.theme = theme; } catch (_) {}
  };
  const observer = new Observer(apply);
  observer.observe(outer.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  media.addEventListener('change', apply);
  frame.addEventListener('load', apply);
  apply();
  return () => { observer.disconnect(); media.removeEventListener('change', apply); frame.removeEventListener('load', apply); };
}
