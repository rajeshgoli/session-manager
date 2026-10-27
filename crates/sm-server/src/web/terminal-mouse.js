// Web-page mouse in the terminal (#2540). tmux with `mouse on` asks the terminal to
// report clicks and drags; a click then lands in tmux, and a drag puts the pane in
// copy mode, where typing drives tmux instead of the agent and a selection goes to
// the server's clipboard. The page keeps clicks and drags for itself: they focus,
// select and copy like any web page. The wheel still reaches tmux, so scrolling
// back through the agent's history works as before.

// DECSET modes that make the terminal report clicks, drags or motion.
const TRACKING = new Set([9, 1000, 1001, 1002, 1003]);

/** The private-mode params left once mouse tracking is taken out, and whether any was there. */
export function withoutTracking(params) {
  const flat = params.map((p) => Array.isArray(p) ? p[0] : p);
  const rest = flat.filter((p) => !TRACKING.has(p));
  return { tracking: rest.length !== flat.length, rest };
}

/** An SGR wheel report (button 64 up, 65 down) at a 1-based cell. */
export function wheelReport(up, col, row) {
  return `\x1b[<${up ? 64 : 65};${col};${row}M`;
}

/**
 * Wheel ticks to report for one wheel event: one per event once a line's worth has
 * built up, the same rate xterm reports at when the application tracks the mouse.
 * `carry` holds the leftover fraction of a line between events.
 */
export function wheelTicks(event, cellHeight, carry) {
  const lines = event.deltaMode === 1 ? event.deltaY : event.deltaMode === 2 ? event.deltaY * 20 : event.deltaY / Math.max(1, cellHeight);
  const total = carry.value + lines;
  if (Math.abs(total) < 1) { carry.value = total; return 0; }
  carry.value = total % 1;
  return total < 0 ? -1 : 1;
}

/**
 * Install on an opened xterm. `send` writes input to the PTY. Returns a disposer.
 * The app's tracking requests are recorded but not applied, so xterm never captures
 * the mouse; while the app wants the mouse, the wheel goes to it as SGR reports.
 */
export function installPageMouse(terminal, host, send, copyText) {
  let wanted = false;
  const carry = { value: 0 };
  const handler = (set) => (params) => {
    const { tracking, rest } = withoutTracking(params);
    if (!tracking) return false;
    wanted = set;
    // Apply whatever else shared the sequence.
    if (rest.length) terminal.write(`\x1b[?${rest.join(';')}${set ? 'h' : 'l'}`);
    return true;
  };
  const on = terminal.parser.registerCsiHandler({ prefix: '?', final: 'h' }, handler(true));
  const off = terminal.parser.registerCsiHandler({ prefix: '?', final: 'l' }, handler(false));
  terminal.attachCustomWheelEventHandler((event) => {
    if (!wanted || event.deltaY === 0) return true;
    const screen = terminal.element?.querySelector('.xterm-screen');
    const box = screen?.getBoundingClientRect();
    const cellW = box ? box.width / terminal.cols : 8, cellH = box ? box.height / terminal.rows : 16;
    const ticks = wheelTicks(event, cellH, carry);
    if (ticks) {
      const col = Math.min(terminal.cols, Math.max(1, Math.floor((event.clientX - (box?.left || 0)) / cellW) + 1));
      const row = Math.min(terminal.rows, Math.max(1, Math.floor((event.clientY - (box?.top || 0)) / cellH) + 1));
      send(wheelReport(ticks < 0, col, row));
    }
    event.preventDefault();
    return false;
  });

  // A Copy button over the terminal while text is selected, for touch screens and
  // anyone who does not reach for the keyboard.
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'btn sm term-copy';
  button.textContent = 'Copy';
  button.hidden = true;
  const copy = async () => {
    const text = terminal.getSelection();
    if (!text) return;
    await copyText(text);
    terminal.clearSelection();
    terminal.focus();
  };
  // Keep the selection: a mousedown on the button would otherwise clear it first.
  button.addEventListener('mousedown', (event) => event.preventDefault());
  button.addEventListener('click', copy);
  host.appendChild(button);
  const selection = terminal.onSelectionChange(() => { button.hidden = !terminal.hasSelection(); });

  return {
    /** For the key handler: Ctrl+Shift+C copies, and so does Ctrl+C off macOS while text is selected. */
    copyKey(event, mac) {
      if (event.type !== 'keydown' || !event.ctrlKey || event.metaKey || event.altKey || event.key.toLowerCase() !== 'c') return false;
      if (!terminal.hasSelection() || (mac && !event.shiftKey)) return false;
      event.preventDefault();
      copy();
      return true;
    },
    dispose() { on.dispose(); off.dispose(); selection.dispose(); button.remove(); },
  };
}
