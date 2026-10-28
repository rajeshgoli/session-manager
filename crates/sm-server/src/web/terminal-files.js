// File links share xterm's link activation, including touch and tmux mouse mode.
// Only tokens with a file extension or an explicit path prefix qualify.
export function fileMatches(text) {
  const pattern = /(?:^|[\s("'`\[=])((?:~\/|\.{1,2}\/|\/)[^\s<>"'`|?*]+|[\w@.-]+(?:\/[\w@.+-]+)*\.[a-zA-Z][\w-]*(?::\d+(?::\d+)?)?)/g;
  const matches = [];
  for (const match of text.matchAll(pattern)) {
    // A compiler diagnostic adds a delimiter colon after its line/column suffix.
    const token = match[1].replace(/[.,;!?)\]}]+$/, '').replace(/(:\d+(?::\d+)?):$/, '$1');
    if (!token || token === '/' || token.endsWith('/')) continue;
    const path = token.replace(/(?::\d+(?::\d+)?|#L\d+(?:C\d+)?)$/, '');
    matches.push({ text: token, path, index: match.index + match[0].length - match[1].length });
  }
  return matches;
}

/** The local path of an OSC 8 `file://` link, which Claude Code uses for file paths; else null. */
export function oscFilePath(uri) {
  try {
    const url = new URL(uri);
    return url.protocol === 'file:' && url.pathname.length > 1 ? decodeURIComponent(url.pathname) : null;
  } catch (_) { return null; }
}

export const fileHref = (id, path) => `/client/terminal/file?session=${encodeURIComponent(id)}&path=${encodeURIComponent(path)}`;

// Web links: xterm's web-links pattern, kept to http(s) URLs that parse.
export function urlMatches(text) {
  const pattern = /https?:\/\/[^\s"'!*(){}|\\^<>`]*[^\s"':,.!?{}|\\^~\[\]`()<>]/gi;
  const matches = [];
  for (const match of text.matchAll(pattern)) {
    try { new URL(match[0]); } catch { continue; }
    matches.push({ text: match[0], index: match.index });
  }
  return matches;
}

// URLs first; a file match inside a URL is part of it.
export function linkMatches(text) {
  const urls = urlMatches(text).map(m => ({ ...m, url: m.text }));
  const files = fileMatches(text).filter(f => !urls.some(u => f.index < u.index + u.text.length && u.index < f.index + f.text.length));
  return [...urls, ...files].sort((a, b) => a.index - b.index);
}

// One row's text and the cell behind each character.
function readRow(buffer, row, last) {
  const line = buffer.getLine(row);
  let text = '';
  const cells = [];
  for (let col = 0; col < line.length; col++) {
    const cell = line.getCell(col);
    if (!cell || cell.getWidth() === 0) continue;
    // xterm leaves a structural blank when a wide character cannot fit
    // in the final column. It belongs to neither the text nor the path.
    const next = col === line.length - 1 && !last ? buffer.getLine(row + 1) : null;
    if (!cell.getChars() && next?.isWrapped && next.getCell(0)?.getWidth() === 2) continue;
    const chars = cell.getChars() || ' ';
    for (let i = 0; i < chars.length; i++) cells.push({ x: col + 1, y: row + 1, width: cell.getWidth() });
    text += chars;
  }
  const trimmed = text.trimEnd();
  const end = trimmed ? cells[trimmed.length - 1].x + cells[trimmed.length - 1].width - 1 : 0;
  return { text, cells, end, indent: text.length - text.trimStart().length };
}

// Claude Code and Codex break their own lines, so a word they split at the
// right edge reaches the next row with no wrap flag, after the row's indent.
// They split a word only when it is longer than a row holds, so a row that
// reaches the edge joins the next row's first word only when together they
// are longer than that row (#2580). A word that merely ends at the edge stays put.
const MAX_INDENT = 8;
const reachesEdge = (row, cols) => row.end >= cols - 1;
const continues = row => row.indent <= MAX_INDENT && row.text.trim() !== '';

export function installFileLinks(terminal, id, { file, url }) {
  return terminal.registerLinkProvider({
    provideLinks(y, callback) {
      const buffer = terminal.buffer.active, cols = terminal.cols;
      const rows = new Map();
      const row = r => {
        if (!rows.has(r)) rows.set(r, readRow(buffer, r, r + 1 >= buffer.length));
        return rows.get(r);
      };
      const wrapped = r => buffer.getLine(r)?.isWrapped;
      const maybeJoined = r => wrapped(r) || (reachesEdge(row(r - 1), cols) && continues(row(r)));
      // Bound hover work.
      let start = y - 1, end = start;
      while (start > 0 && y - start < 64 && maybeJoined(start)) start--;
      while (end + 1 < buffer.length && end - start < 64 && maybeJoined(end + 1)) end++;
      const lines = [];
      let text = '', cells = [];
      for (let r = start; r <= end; r++) {
        const current = row(r);
        if (r > start && !wrapped(r)) {
          const before = row(r - 1);
          const kept = text.trimEnd(), tail = kept.length - kept.replace(/\S+$/, '').length;
          const head = current.text.slice(current.indent).match(/^\S*/)[0].length;
          if (tail > 0 && reachesEdge(before, cols) && continues(current) && tail + head > before.end - current.indent) {
            text = text.trimEnd() + current.text.slice(current.indent);
            cells = cells.slice(0, text.length - current.text.length + current.indent).concat(current.cells.slice(current.indent));
            continue;
          }
          lines.push({ text, cells });
          text = ''; cells = [];
        }
        text += current.text;
        cells = cells.concat(current.cells);
      }
      lines.push({ text, cells });
      callback(lines.flatMap(({ text, cells }) => linkMatches(text).flatMap(match => {
        const first = cells[match.index], last = cells[match.index + match.text.length - 1];
        if (!first || !last || y < first.y || y > last.y) return [];
        return [{ text: match.text, range: { start: { x: first.x, y: first.y }, end: { x: last.x + last.width - 1, y: last.y } },
          activate: event => match.url ? url(event, match.url) : file(event, fileHref(id, match.path)) }];
      })));
    },
  });
}
