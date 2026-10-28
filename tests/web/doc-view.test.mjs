// The document view (2543 F; tests I.5): every opener lands on /read/…, old ?open=doc: links
// are rewritten, no doc panel remains, and the replies note moves into the document bar.
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync, readdirSync} from 'node:fs';
import vm from 'node:vm';

const web = 'crates/sm-server/src/web/';
const read = name => readFileSync(web + name, 'utf8');
const slice = (source, from, to) => source.slice(source.indexOf(from), source.indexOf(to));
const ui = read('ui.js');
const reader = read('reader.js');
const app = read('app.js');
const helpers = slice(ui, '/** A same-origin reader path', '/** Open external handoff links');
const origin = 'https://sm.example.com';

function opener(extra = {}) {
  const navigated = [], opened = [], panels = [];
  const context = {URL, URLSearchParams, location: {origin}, navigate: p => navigated.push(p),
    openPanel: p => panels.push(p), window: {open: (...args) => opened.push(args)}, ...extra};
  vm.runInNewContext(helpers.replaceAll('export ', ''), context);
  return {context, navigated, opened, panels};
}

test('openers in Inbox, History, the reader, ui.js and the palette navigate to /read', () => {
  // No opener anywhere still targets the side panel, and the doc panel is gone (F.4).
  for (const name of readdirSync(web).filter(f => f.endsWith('.js'))) {
    const source = read(name);
    assert.doesNotMatch(source, /openPanel\(`doc:/, name);
    assert.doesNotMatch(source, /registerPanel\('doc'/, name);
    assert.doesNotMatch(source, /openItem\('doc'/, name);
  }
  for (const [name, count] of [['inbox.js', 6], ['history.js', 1], ['reader.js', 1], ['terminal.js', 1], ['app.js', 1]]) {
    assert.equal((read(name).match(/openReader\(/g) || []).length >= count, true, `${name} opens documents in the document view`);
  }
  assert.match(read('autopm.js'), /href=\$\{readHref\(view\.readout\.reader_path\)\}/);

  const {context, navigated, opened, panels} = opener();
  const openDoc = slice(ui, 'export function openDoc(', '/** Shared related-item chips.').replace('export ', '');
  vm.runInContext(`${openDoc}
    openDoc('/docs/~handoff/agent');
    openDoc('${origin}/docs/repo/a.html?version=abc#sm-review');
    openDoc('https://github.com/a/b/issues/1');
    openDoc('javascript:alert(1)');`, context);
  assert.deepEqual(navigated, ['/read/docs/~handoff/agent', '/read/docs/repo/a.html?version=abc#sm-review']);
  assert.deepEqual(opened, [['https://github.com/a/b/issues/1', '_blank', 'noopener']]);
  assert.deepEqual(panels, []);
});

test('readerPath accepts only same-origin reader paths', () => {
  const {context} = opener();
  assert.equal(context.readerPath('/docs/x?version=1#a'), '/docs/x?version=1#a');
  assert.equal(context.readerPath(`${origin}/messages/m1`), '/messages/m1');
  assert.equal(context.readerPath('/t/repo/1'), '/t/repo/1');
  assert.equal(context.readerPath('//evil.example.com/docs/x'), null);
  assert.equal(context.readerPath('https://evil.example.com/docs/x'), null);
  assert.equal(context.readerPath('/board'), null);
});

test('terminal web and file links open sm documents in the document view, others in a tab', () => {
  const source = read('terminal.js');
  const links = slice(source, 'const openTarget = ', 'const session = ');
  const navigated = [], opened = [];
  const context = {URL, location: {origin}, window: {open: (...args) => opened.push(args)}};
  vm.runInNewContext(`${helpers.replaceAll('export ', '')}\n${links}\nthis.openLink = openLink;`,
    Object.assign(context, {navigate: p => navigated.push(p)}));
  let prevented = 0;
  const event = {preventDefault: () => prevented++};
  context.openLink(event, `${origin}/docs/repo/memo.html`);
  context.openLink(event, 'https://example.org/page');
  context.openLink(event, 'file:///etc/passwd');
  assert.deepEqual(navigated, ['/read/docs/repo/memo.html']);
  assert.deepEqual(opened, [['https://example.org/page', '_blank', 'noopener,noreferrer']]);
  assert.equal(prevented, 2);
  assert.match(source, /installFileLinks\(terminal, id, \(event, uri\) => \{\s+event\?\.preventDefault\?\.\(\);\s+openTarget\(uri\);/);
});

test('/read routes to the document view and old ?open=doc: links are rewritten on arrival', () => {
  const pageFor = slice(app, 'function pageFor(', 'function readLocation(');
  const readLocation = slice(app, 'function readLocation(', 'function urlFor(');
  const visit = (pathname, search, hash = '') => {
    const location = {pathname, search, hash};
    const history = {state: {smHistoryIndex: 0}, replaceState(state, title, url) {
      const next = new URL(url, origin); location.pathname = next.pathname; location.search = next.search; location.hash = next.hash;
    }};
    const context = vm.createContext({location, history, URL, URLSearchParams, PAGES: [{key: 'inbox', path: '/inbox'}]});
    vm.runInContext(`${pageFor}\n${readLocation}\nthis.pageFor = pageFor; this.readLocation = readLocation;`, context);
    return {context, location};
  };
  const plain = visit('/read/docs/x', '?version=1');
  assert.equal(plain.context.pageFor('/read/docs/x'), 'doc');
  assert.deepEqual({...plain.context.readLocation()}, {path: '/read/docs/x', open: null});

  const old = visit('/inbox', '?open=doc:/docs/repo/memo.html%3Fversion%3Dabc', '#sm-review');
  assert.deepEqual({...old.context.readLocation()}, {path: '/read/docs/repo/memo.html', open: null});
  assert.equal(old.location.search, '?version=abc');
  assert.equal(old.location.hash, '#sm-review');
  // Other panels are untouched.
  assert.equal(visit('/board', '?open=agent:abc').context.readLocation().open, 'agent:abc');
});

test('the document address keeps its query but drops panel keys and the fragment', () => {
  const context = {URLSearchParams};
  vm.runInNewContext(slice(reader, 'export function docPathFrom(', '/** ← and ⌘[').replace('export ', ''), context);
  assert.equal(context.docPathFrom('/read/docs/r/2543.html', '?version=291f65d4bbea'), '/docs/r/2543.html?version=291f65d4bbea');
  assert.equal(context.docPathFrom('/read/docs/r/2543.html', '?version=291f&open=ticket:o/r%231'), '/docs/r/2543.html?version=291f');
  assert.equal(context.docPathFrom('/read/messages/m1', ''), '/messages/m1');
});

test('back returns to the previous in-app entry, else the landing; revisions count oldest = 1', () => {
  const calls = [];
  const context = {history: {back: () => calls.push('back')}, navigate: p => calls.push(p)};
  vm.runInNewContext(slice(reader, 'export function docBack(', 'export const repliesText').replaceAll('export ', ''), context);
  context.docBack({smHistoryIndex: 3});
  context.docBack({smHistoryIndex: 0});
  context.docBack(null);
  assert.deepEqual(calls, ['back', '/', '/']);
  const revisions = [{sha: 'c'}, {sha: 'b'}, {sha: 'a'}];
  assert.equal(context.revisionNumber(revisions, 'c'), 3);
  assert.equal(context.revisionNumber(revisions, 'a'), 1);
  assert.equal(context.revisionNumber(revisions, 'z'), null);
  vm.runInNewContext(`${slice(reader, 'export const repliesText', 'const when = ').replace('export ', '')}\nthis.repliesText = repliesText;`, context);
  assert.equal(context.repliesText(1), '1 reply on changed passages');
  assert.equal(context.repliesText(2), '2 replies on changed passages');
});

test('the author chip finds the live author by id or name, else points at History', () => {
  const context = {};
  vm.runInNewContext(slice(reader, 'export function authorOf(', '/** `id` is the reader path').replace('export ', ''), context);
  const live = {id: 's1', name: 'sm-2543', state: 'idle', attention: {section: 'moving'}};
  assert.equal(context.authorOf({authorSessionId: 's1', authorAgent: 'sm-2543'}, [live]).live, true);
  assert.equal(context.authorOf({authorSessionId: 'gone', authorAgent: 'sm-2543'}, [live]).id, 's1');
  const stopped = context.authorOf({authorSessionId: 's9', authorAgent: 'sm-old'}, [live]);
  assert.deepEqual({...stopped, session: undefined}, {id: 's9', name: 'sm-old', session: undefined, live: false});
  assert.equal(context.authorOf({title: 'A message'}, [live]), null);
});

test('the document view drops Ask and full screen, and puts Review last in the bar', () => {
  assert.doesNotMatch(reader, /Ask|doc-ask|fullScreen|ask-target/);
  const bar = slice(reader, '<header class="bar doc-bar">', '</header>');
  const order = ['Back', 'doc-title', 'Revision ', 'doc-hold', 'doc-replies', 'doc-note', 'doc-author', 'Open in Claude', '⎙ Print', 'title="More"', '${moon}', 'BugButton', 'No review needed', 'reviewLabel(review?.drafts)'];
  const at = order.map(item => bar.indexOf(item));
  assert.ok(at.every(i => i >= 0), JSON.stringify(Object.fromEntries(order.map((k, i) => [k, at[i]]))));
  assert.deepEqual([...at].sort((a, b) => a - b), at);
});

test('the review client counts unseen changed-passage replies for the app and leaves its banner line out when framed', () => {
  const source = readFileSync('crates/sm-server/src/http/doc_client.js', 'utf8');
  const note = slice(source, '  function noteReplies(', '  function showChangedThreads(');
  const posts = [];
  const make = framed => {
    const context = {S: {repliesUnseen: 0}, framed, location: {origin}, window: {parent: {postMessage: (...args) => posts.push(args)}}};
    vm.runInNewContext(`${note}\nthis.noteReplies = noteReplies;`, context);
    return context;
  };
  const inApp = make(true);
  inApp.noteReplies(2); inApp.noteReplies(2); inApp.noteReplies(0);
  assert.equal(inApp.S.repliesUnseen, 0);
  assert.deepEqual(posts.map(p => [p[0].type, p[0].count, p[1]]), [['sm-doc-replies', 2, origin], ['sm-doc-replies', 0, origin]]);
  const direct = make(false);
  direct.noteReplies(3);
  assert.equal(direct.S.repliesUnseen, 3);
  assert.equal(posts.length, 2);
  // The banner line is kept on a direct /docs/… load only.
  assert.match(source, /noteReplies\(unseen\);[\s\S]{0,120}if \(unseen && !framed\) \{/);
  assert.match(source, /S\.showChangedThreads = showChangedThreads;/);
});

test('inside the app the review client draws no toolbar or pill and hands its state to the document bar (sm#2568)', () => {
  const source = readFileSync('crates/sm-server/src/http/doc_client.js', 'utf8');
  // Layout hides both the bar and its folded pill when framed; a direct load keeps them.
  const layout = slice(source, '  function layout() {', '  // Fixed elements are placed');
  const run = framed => {
    const classes = new Set(['sm-doc-bar-open']);
    const context = {framed, collapsed: true, bar: {hidden: false, offsetHeight: 40}, pill: {hidden: false}, banner: {style: {}},
      document: {documentElement: {style: {setProperty() {}}, classList: {remove: c => classes.delete(c), toggle: (c, on) => on ? classes.add(c) : classes.delete(c)}}}};
    vm.runInNewContext(`${layout}\nlayout();`, context);
    return {bar: context.bar.hidden, pill: context.pill.hidden, open: classes.has('sm-doc-bar-open')};
  };
  assert.deepEqual(run(true), {bar: true, pill: true, open: false});
  assert.deepEqual(run(false), {bar: true, pill: false, open: false});
  assert.match(source, /function renderBar\(\) \{\s+clear\(bar\);\s+if \(framed\) \{ layout\(\); postBar\(\); return; \}/);

  const state = slice(source, '  function barState() {', '  function dismissReview() {');
  const posting = source.match(/  function posting\(sub\) \{[^\n]*\n/)[0];
  const posts = [];
  const make = S => {
    const context = {S, MSG: false, CONFIG: {prNumber: 7, prUrl: 'https://github.com/o/r/pull/7', revisions: [{sha: 'b'}, {sha: 'a'}]},
      currentDrafts: () => [{}, {}], location: {origin}, window: {parent: {postMessage: (...args) => posts.push(args)}}, Date};
    vm.runInNewContext(`${posting}${state}\nthis.barState = barState; this.postBar = postBar;`, context);
    return context;
  };
  const held = make({canComment: true, docState: 'review_requested', mergeHold: {placed_by: 'sm-1', reason: 'waiting'}, prState: 'open', sentUntil: 0, sub: null});
  assert.deepEqual(JSON.parse(JSON.stringify(held.barState())), {drafts: 2, canReview: true, reviewRequested: true,
    mergeHold: {placed_by: 'sm-1', reason: 'waiting'}, prNumber: 7, prUrl: 'https://github.com/o/r/pull/7', prState: 'open',
    revisions: [{sha: 'b'}, {sha: 'a'}], note: null});
  held.postBar();
  assert.equal(posts[0][0].type, 'sm-doc-bar'); assert.equal(posts[0][1], origin); assert.equal(posts[0][0].state.drafts, 2);
  const note = S => make({canComment: true, prState: 'open', sentUntil: 0, ...S}).barState().note;
  assert.deepEqual({...note({sub: {status: 'failed', error: 'HTTP 422'}})}, {kind: 'failed', text: 'Review not posted: HTTP 422'});
  assert.equal(note({sentUntil: Date.now() + 4000, sub: {status: 'queued', queued_at: new Date().toISOString()}}).kind, 'sent');
  assert.equal(note({sub: {status: 'submitting', queued_at: new Date(Date.now() - 61000).toISOString()}}).kind, 'posting');
  assert.equal(note({sub: {status: 'queued', queued_at: new Date().toISOString()}}), null);
  for (const action of ['S.dismissReview = dismissReview', 'S.toggleHold = toggleHold', 'S.resubmit = ', 'S.discardSubmission = ', 'S.barState = barState']) {
    assert.ok(source.includes(action), action);
  }
});

test('Review counts draft comments; Hold merge sits in the ⋯ menu while the PR is open', () => {
  const context = {};
  vm.runInNewContext(`${slice(reader, 'export const reviewLabel', 'export const repliesText').replace('export ', '')}\nthis.reviewLabel = reviewLabel;`, context);
  assert.equal(context.reviewLabel(0), 'Review');
  assert.equal(context.reviewLabel(undefined), 'Review');
  assert.equal(context.reviewLabel(3), 'Review (3)');
  const menu = slice(reader, "${menu === 'more'", '${moon}');
  assert.match(menu, /info\?\.prState === 'open'[\s\S]*hold \? 'Release hold' : 'Hold merge'/);
});
