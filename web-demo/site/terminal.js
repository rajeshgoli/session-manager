// The demo's agent terminals. The real page opens a WebSocket to the agent's
// tmux pane; a service worker can't answer one, so demo.js hands sockets for
// /__demo/terminal/<id> to SmDemoTerminal instead. It speaks the same frames
// as the server (auth, status, output, ping/pong) and draws a Claude Code or
// Codex screen: the agent's script from terminals.json up to the storyline's
// current moment, its live status and context from the recording, and an
// input box the visitor can type into.
(function () {
  'use strict';
  const CSI = '\x1b[';
  const SPIN = ['·', '✢', '✳', '✶', '✻', '✽', '✻', '✶', '✳', '✢'];
  const POLL_MS = 2000;
  const SPIN_MS = 120;
  // Captured from real Claude Code and Codex panes (256-colour codes).
  const STYLE = {
    text: '38;5;231', dim: '38;5;246', rule: '38;5;244', ok: '38;5;114', spin: '38;5;174',
    userBg: '48;5;237', userMark: '38;5;239', auto: '38;5;220', code: '38;5;153', faint: '2',
  };
  const MODELS = { fable: 'Fable 5.1', opus: 'Opus 5.5 (1M context)', 'opus[1m]': 'Opus 5.5 (1M context)', sonnet: 'Sonnet 5.5' };
  const paint = (style, text) => `${CSI}${style}m${text}${CSI}0m`;
  const visible = (text) => text.replace(/\*\*|`/g, '');

  let scriptsPromise = null;
  const scripts = () => scriptsPromise || (scriptsPromise = fetch('/demo/terminals.json').then((r) => r.json()).catch(() => ({})));

  /** Word-wrap `text` to `width` visible characters (markup doesn't count). */
  function wrap(text, width) {
    const lines = [];
    for (const paragraph of String(text).split('\n')) {
      let line = '';
      for (let word of paragraph.split(' ')) {
        while (visible(word).length > width) {
          if (line) { lines.push(line); line = ''; }
          lines.push(word.slice(0, width)); word = word.slice(width);
        }
        const next = line ? `${line} ${word}` : word;
        if (visible(next).length > width && line) { lines.push(line); line = word; } else line = next;
      }
      lines.push(line);
    }
    return lines;
  }

  /** **bold** and `code` across wrapped lines; `base` is the line's own colour. */
  function markup(lines, base) {
    let bold = false;
    let code = false;
    return lines.map((line) => {
      let out = (bold ? `${CSI}1m` : '') + (code ? `${CSI}${STYLE.code}m` : '');
      out += line.replace(/\*\*|`/g, (mark) => {
        if (mark === '**') { bold = !bold; return bold ? `${CSI}1m` : `${CSI}22m`; }
        code = !code;
        return code ? `${CSI}${STYLE.code}m` : `${CSI}${base}m`;
      });
      return out + `${CSI}0m`;
    });
  }

  function hanging(first, rest, text, width, base) {
    return markup(wrap(text, width - visible(rest).length), base)
      .map((line, i) => `${i ? rest : first}${CSI}${base}m${line}`);
  }

  // ---- one step of the transcript, as lines --------------------------------------

  function claudeStep(step, width) {
    if (step.user) {
      return [...wrap(step.user, width - 2).map((line, i) => `${CSI}${STYLE.userBg}m${i ? '  ' : paint(`${STYLE.userBg};${STYLE.userMark}`, '❯ ') + CSI + STYLE.userBg + 'm'}`
        + `${CSI}${STYLE.text}m${line.padEnd(width - 2)}${CSI}0m`), ''];
    }
    if (step.say) return [...hanging(paint(STYLE.text, '⏺ '), '  ', step.say, width, STYLE.text), ''];
    if (step.tool) {
      const at = step.tool.indexOf('(');
      const name = at < 0 ? step.tool : step.tool.slice(0, at);
      const head = hanging(paint(STYLE.ok, '⏺ '), '  ', step.tool, width, STYLE.text);
      head[0] = head[0].replace(name, `${CSI}1m${name}${CSI}22m`);
      const out = (step.out || []).flatMap((line, i) => hanging(i ? '     ' : paint(STYLE.dim, '  ⎿  '), '     ', line, width, STYLE.dim));
      return [...head, ...out, ''];
    }
    if (step.done) return [paint(STYLE.dim, `✻ ${step.done}`), ''];
    return [];
  }

  function codexStep(step, width) {
    if (step.user) return [...hanging(paint(`1;${STYLE.faint}`, '› '), '  ', step.user, width, '0'), ''];
    if (step.say) return [...hanging(paint(STYLE.faint, '• '), '  ', step.say, width, '0'), ''];
    if (step.tool) {
      const verb = step.tool.split(' ')[0];
      const head = hanging(`${CSI}1;32m•${CSI}0m `, '  ', step.tool, width, '0');
      head[0] = head[0].replace(verb, `${CSI}1m${verb}${CSI}22m`);
      const out = (step.out || []).flatMap((line, i) => hanging(paint(STYLE.faint, i ? '    ' : '  └ '), '    ', line, width, STYLE.faint));
      return [...head, ...out, ''];
    }
    if (step.done) return [paint(STYLE.faint, `  ${step.done}`), ''];
    return [];
  }

  // ---- the socket -----------------------------------------------------------------

  class SmDemoTerminal {
    constructor(url) {
      this.url = url.href;
      this.id = decodeURIComponent(url.pathname.split('/').pop());
      this.readyState = 0;
      this.onopen = this.onmessage = this.onclose = this.onerror = null;
      this.cols = 100; this.rows = 30;
      this.sequence = 0;
      this.shown = []; this.fromScript = 0; this.lastT = null;
      this.session = null; this.script = null;
      this.input = ''; this.thinking = null; this.blockHeight = 0; this.drawn = false;
      setTimeout(() => { this.readyState = 1; this.onopen && this.onopen({}); }, 30);
    }

    send(text) {
      if (this.readyState !== 1) return;
      let frame;
      try { frame = JSON.parse(text); } catch (_) { return; }
      if (frame.type === 'auth') this.attach();
      else if (frame.type === 'resize') { this.cols = frame.cols; this.rows = frame.rows; if (this.drawn) this.full(); }
      else if (frame.type === 'ping') this.emit({ type: 'pong', id: frame.id });
      else if (frame.type === 'input') this.type(frame.data);
      else if (frame.type === 'key') this.key(frame.key);
      else if (frame.type === 'detach') this.close();
    }

    close() {
      if (this.readyState === 3) return;
      this.readyState = 3;
      clearInterval(this.poller); clearInterval(this.spinner); clearTimeout(this.thinking);
      setTimeout(() => this.onclose && this.onclose({ code: 1000, reason: '' }), 0);
    }

    emit(frame) {
      setTimeout(() => { if (this.readyState === 1 && this.onmessage) this.onmessage({ data: JSON.stringify(frame) }); }, 0);
    }

    write(text) {
      const bytes = new TextEncoder().encode(text);
      let binary = '';
      for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
      this.emit({ type: 'output', sequence: ++this.sequence, data: btoa(binary) });
    }

    async attach() {
      this.emit({ type: 'status', state: 'attached' });
      await this.poll();
      this.full();
      this.poller = setInterval(() => this.poll().then((changed) => changed && this.redraw()), POLL_MS);
      this.spinner = setInterval(() => { if (this.working()) this.drawBlock(); }, SPIN_MS);
    }

    // ---- data ---------------------------------------------------------------------

    async poll() {
      const [clock, watch, all] = await Promise.all([
        fetch('/__demo/clock').then((r) => (r.ok ? r.json() : null)).catch(() => null),
        fetch(`/watch/state?session=${encodeURIComponent(this.id)}`).then((r) => (r.ok ? r.json() : null)).catch(() => null),
        scripts(),
      ]);
      const session = watch && (watch.sessions || []).find((s) => s.id === this.id);
      const before = JSON.stringify([this.session && [this.session.status_text, this.session.activity_state, this.session.context_percent]]);
      if (session) this.session = session;
      if (!this.session) return false;
      this.script = all[this.session.name] || { cwd: this.session.working_dir, steps: [] };
      const t = clock ? clock.t : 0;
      let changed = before !== JSON.stringify([[this.session.status_text, this.session.activity_state, this.session.context_percent]]);
      if (this.lastT !== null && t + 5 < this.lastT) {
        // The loop started again: so does the conversation.
        this.shown = []; this.fromScript = 0; this.pendingFull = true; changed = true;
      }
      this.lastT = t;
      const steps = this.script.steps;
      while (this.fromScript < steps.length && steps[this.fromScript].t <= t) {
        this.shown.push(steps[this.fromScript++]); this.added = (this.added || 0) + 1; changed = true;
      }
      return changed;
    }

    codex() { return this.session && this.session.provider === 'codex'; }

    working() {
      if (this.thinking) return true;
      const s = this.session;
      return !!s && s.activity_state !== 'idle' && s.state !== 'stopped' && !(s.facts && s.facts.finished);
    }

    // ---- drawing --------------------------------------------------------------------

    lines(steps) {
      const width = Math.max(20, this.cols);
      return steps.flatMap((step) => (this.codex() ? codexStep(step, width) : claudeStep(step, width)));
    }

    block() {
      const width = Math.max(20, this.cols);
      const s = this.session || {};
      const pct = Math.round(s.context_percent || 0);
      const seconds = Math.max(0, Math.floor((Date.now() - Date.parse(this.thinkingSince || s.activity_since || s.status_at || Date.now())) / 1000));
      const frame = SPIN[Math.floor(Date.now() / SPIN_MS) % SPIN.length];
      const status = this.thinking ? 'Thinking' : (s.status_text || 'Working');
      const caret = `${CSI}7m ${CSI}27m`;
      const out = [];
      if (this.codex()) {
        if (this.working()) out.push(`${CSI}1m•${CSI}22m ${CSI}1m${status}${CSI}22m ${paint(STYLE.faint, `(${seconds}s • esc to interrupt)`)}`, '');
        const input = this.input ? wrap(this.input, width - 2) : [''];
        input.forEach((line, i) => out.push(`${i ? '  ' : `${CSI}1m›${CSI}22m `}${line}${i === input.length - 1 ? (this.input ? caret : `${caret}${paint(STYLE.faint, 'Ask Codex to do anything')}`) : ''}`));
        out.push('');
        const model = `${s.model || 'gpt-5.5'} ${s.reasoning_effort || 'high'}`;
        out.push(`  ${paint('38;5;223', model)} · ${paint('38;5;151', this.script.cwd || '~')} · ${paint('38;5;216', `Context ${pct}% used`)}`);
        out.push(`  ${CSI}1m?${CSI}22m for shortcuts`);
        return out;
      }
      if (this.working()) out.push(`${paint(STYLE.spin, `${frame} ${status}…`)} ${paint(STYLE.dim, `(${seconds}s · esc to interrupt)`)}`, '');
      out.push(paint(STYLE.rule, '─'.repeat(width)));
      const input = wrap(this.input, width - 2);
      input.forEach((line, i) => out.push(`${i ? '  ' : '❯ '}${line}${i === input.length - 1 ? caret : ''}`));
      out.push(paint(STYLE.rule, '─'.repeat(width)));
      const filled = Math.round(pct / 5);
      out.push(`  ${paint(STYLE.dim, MODELS[s.model] || s.model || 'Opus 5.5')}  ${paint('32', `[${'█'.repeat(filled)}${'░'.repeat(20 - filled)}]`)} ${paint(STYLE.dim, `${pct}%`)}`);
      out.push(`  ${paint(STYLE.auto, '⏵⏵ auto mode on')}${paint(STYLE.dim, ' (shift+tab to cycle) · ← for agents')}`);
      return out;
    }

    clearBlock() {
      return (this.blockHeight > 1 ? `${CSI}${this.blockHeight - 1}A` : '') + `\r${CSI}J`;
    }

    full() {
      if (!this.session) return;
      this.drawn = true; this.pendingFull = false; this.added = 0;
      const block = this.block();
      const body = this.lines(this.shown);
      // A full reset (RIS): an erase would push the old screen into scrollback.
      this.write(`\x1bc${CSI}?25l${[...body, ...block].join('\r\n')}`);
      this.blockHeight = block.length;
    }

    redraw() {
      if (this.pendingFull || !this.drawn) { this.full(); return; }
      const added = this.added || 0;
      this.added = 0;
      const fresh = added ? this.lines(this.shown.slice(-added)) : [];
      const block = this.block();
      this.write(this.clearBlock() + [...fresh, ...block].join('\r\n'));
      this.blockHeight = block.length;
    }

    drawBlock() { if (this.drawn) this.redraw(); }

    // ---- the visitor's typing ----------------------------------------------------------

    type(data) {
      if (!this.drawn) return;
      let text = data.replace(/\x1b\[200~([\s\S]*?)\x1b\[201~/g, (_, pasted) => pasted.replace(/\r?\n/g, ' '));
      for (const ch of text.replace(/\x1b\[[0-9;?]*[A-Za-z~]|\x1bO[A-Za-z]/g, '')) {
        if (ch === '\r') this.submit();
        else if (ch === '\x7f' || ch === '\b') this.input = [...this.input].slice(0, -1).join('');
        else if (ch === '\x03' || ch === '\x15') this.input = '';
        else if (ch >= ' ' && this.input.length < 2000) this.input += ch;
      }
      this.drawBlock();
    }

    key(key) {
      if (key === 'escape' || key === 'ctrl-c') { this.input = ''; this.drawBlock(); }
    }

    submit() {
      const text = this.input.trim();
      this.input = '';
      if (!text || this.thinking) return;
      const name = (this.session && this.session.name) || 'this agent';
      this.shown.push({ user: text }); this.added = (this.added || 0) + 1;
      this.thinkingSince = new Date().toISOString();
      this.thinking = setTimeout(() => {
        this.thinking = null;
        const tool = this.codex() ? 'Codex' : 'Claude Code';
        this.shown.push(
          { say: `This is the demo, so ${name} is a recording and can't act on that. With Session Manager installed, what you type here goes straight to the agent's real ${tool} terminal, from your desk or your phone.` },
          { done: 'Worked for 2s' });
        this.added += 2;
        this.redraw();
        window.dispatchEvent(new CustomEvent('sm-demo-notice', {
          detail: `In Session Manager this reaches ${name}'s real terminal. Install Session Manager to try it.`,
        }));
      }, 1600);
      this.redraw();
    }
  }

  SmDemoTerminal.CONNECTING = 0; SmDemoTerminal.OPEN = 1; SmDemoTerminal.CLOSING = 2; SmDemoTerminal.CLOSED = 3;
  window.SmDemoTerminal = SmDemoTerminal;
  if (typeof module !== 'undefined') module.exports = { wrap, markup, claudeStep, codexStep };
})();
