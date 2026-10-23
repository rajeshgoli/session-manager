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

export const fileHref = (id, path) => `/client/terminal/file?session=${encodeURIComponent(id)}&path=${encodeURIComponent(path)}`;

export function installFileLinks(terminal, id, activate) {
  return terminal.registerLinkProvider({
    provideLinks(y, callback) {
      const buffer = terminal.buffer.active;
      let start = y - 1, end = start;
      // Join soft-wrapped rows, never separate output lines. Bound hover work.
      while (start > 0 && buffer.getLine(start)?.isWrapped && y - start < 64) start--;
      while (end + 1 < buffer.length && buffer.getLine(end + 1)?.isWrapped && end - start < 64) end++;
      let text = '';
      const cells = [];
      for (let row = start; row <= end; row++) {
        const line = buffer.getLine(row);
        for (let col = 0; col < line.length; col++) {
          const cell = line.getCell(col);
          if (!cell || cell.getWidth() === 0) continue;
          // xterm leaves a structural blank when a wide character cannot fit
          // in the final column. It belongs to neither the text nor the path.
          const next = col === line.length - 1 && row < end ? buffer.getLine(row + 1) : null;
          if (!cell.getChars() && next?.isWrapped && next.getCell(0)?.getWidth() === 2) continue;
          const chars = cell.getChars() || ' ';
          for (let i = 0; i < chars.length; i++) cells.push({ x: col + 1, y: row + 1, width: cell.getWidth() });
          text += chars;
        }
      }
      callback(fileMatches(text).flatMap(match => {
        const first = cells[match.index], last = cells[match.index + match.text.length - 1];
        if (!first || !last || y < first.y || y > last.y) return [];
        return [{ text: match.text, range: { start: { x: first.x, y: first.y }, end: { x: last.x + last.width - 1, y: last.y } },
          activate: event => activate(event, fileHref(id, match.path)) }];
      }));
    },
  });
}
