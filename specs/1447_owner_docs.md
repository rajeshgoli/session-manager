# sm#1447: Owner docs — publish, read, and review agent-written docs from sm

## Problem

Agents often write docs (decision memos, readouts) for the owner to read. Today:

- On the studio, the owner opens the file in a browser. Anywhere else it is hard to find.
- On a laptop, the owner runs `python -m http.server` in `~` and hunts for the path.
- On mobile, GitHub neither renders the HTML nor lets you download it, so the doc
  can't be read at all.
- Reviewing means finding the passage in the PR's file view and adding inline
  comments there. That is clumsy on a laptop and impossible on mobile.

The session expansion in the app already shows status, what the agent is owed, and
what it's waiting for. Docs written for the owner should get the same first-class
treatment.

## Design principles

1. **Git is the store.** A doc is a file at a commit in a GitHub repo. sm stores
   pointers (repo, path, PR, SHA) and never copies or versions file bytes. Durability,
   history and diffs all come from git.
2. **GitHub is the review system.** Comments are GitHub PR review comments. sm is a
   better front end for writing them and wakes the agent when they're posted, the same
   way it does for Codex reviews (`[sm review] ...`, see `render_codex_review_landed_message`
   in `http.rs`).
3. **No extra sandboxing.** Agents already run semi-trusted with arbitrary code
   execution on this machine. The doc is served as-is behind the existing server auth
   (Google browser session / device auth). Do not build iframe sandboxes or CSP
   isolation.
4. **Reading always works; commenting needs an open PR.** A doc tied only to a commit,
   or whose PR is closed or merged, is read-only.

## Agent contract (add to AGENTS.md)

Add this section to `AGENTS.md` verbatim (in this repo, and wherever the shared
agent instructions are templated):

```markdown
## Docs for the owner

If you need the owner to read a doc (memo, readout, decision doc), it must be in git
and published with `sm doc publish`. Don't just leave it on disk or mention a path.

- Read only: commit and push the file, then `sm doc publish <path>`. This pins the
  pushed HEAD commit.
- Needs the owner's review: open a PR containing the file, then
  `sm doc publish <path> --pr <N> --review`. The review arrives as a GitHub PR review,
  and sm wakes you with `[sm review] Rajesh's review of "<title>" ... is here: <url>`.
  Read the review with `gh`, address the comments, push, and run `sm doc publish`
  again so the owner sees the new revision.
- Prefer self-contained HTML (inline CSS, images as data URIs). Markdown also works.
```

## User-facing surfaces

### CLI

```bash
sm doc publish <path> [--pr N | --commit SHA] [--title "..."] [--note "..."] [--review]
sm doc list [--session <id>] [--json]          # default: the caller's session tree
sm doc show <doc-id> [--json]                   # metadata, revisions, reviews, URLs
sm doc retract <doc-id>                         # hide from reading lists (does not touch git)
```

`sm doc publish` behaviour (the resolution happens in the CLI, which runs in the
agent's cwd):

1. Resolve the git repo root containing `<path>`, the repo-relative path, and the repo
   slug (`owner/name`, from `gh repo view --json nameWithOwner` or by parsing the
   `origin` URL).
2. Resolve the commit:
   - `--pr N`: fetch the PR (`gh pr view N --json headRefOid,state,files`). The pinned
     SHA is the PR head. If local `HEAD` differs from the PR head, or the working-tree
     file differs from the file at the PR head, print a warning ("the owner will see
     the pushed version at <sha7>; push first if that's not what you want"). Fail if
     the PR doesn't contain the file at its head (a 404 from the contents API).
   - `--commit SHA`: use it as given.
   - Neither: use `HEAD`. Fail with "commit and push first" if the file has
     uncommitted changes, or if `HEAD` isn't on any remote-tracking branch
     (`git branch -r --contains HEAD` is empty).
3. `--review` requires `--pr` and an open PR.
4. POST to the server with the author session from `CLAUDE_SESSION_MANAGER_ID`.
5. Print `Published "<title>" (<doc-id>) at <sha7> → <reader URL>`.

Identity and republishing: a doc is keyed by `(repo, path, pr_number)`, or by
`(repo, path)` when there's no PR. Publishing the same key again adds a new
**publish event** to the existing doc; it doesn't create a second doc. The latest
publish carries the current title, note and review request.

### Android app

- **Session expansion** (`AgentWorkSections` in `WatchScreen.kt`): add a **Docs**
  surface before "Reviews". One row per doc authored by the session: title, state chip
  (`New` / `Updated` / `Review requested` / `Reviewed` / `Read`) and relative publish
  time. Tapping opens the reader.
- No global reading list here. Docs attach to the session that wrote them. A
  cross-cutting view of which agent worked on which ticket, opened which PR, and wrote
  which doc is the history page in the separate ticket/PR-claims work (see "Related
  work").
- **Reader**: a full-screen WebView loading `/docs/{id}/view` with the same auth the
  app uses for API calls (pass the device auth headers on `loadUrl`). All review UI
  lives inside the served page (below), so Android needs no native comment UI; one
  implementation serves every client. Enable JavaScript and DOM storage, and handle
  back navigation.

### Web (laptop / studio browser)

The reader is a plain server page behind the existing browser-session (Google) auth.
`GET /docs/{id}` redirects to `/docs/{id}/view?sha=<latest>`, so any doc link works in
a laptop browser. The Rust server serves no web session list today (`web/sm-watch` is
not mounted), so on a laptop you find docs through the `sm watch` expansion and
`sm doc list`, which print reader URLs. If a web session view lands later, it gets
the same Docs row as Android, fed by the same projection.

### `sm watch` TUI

Show a `📄N` marker on a session row when it has unread docs or docs awaiting review
(match existing marker conventions; use an ASCII fallback if the TUI avoids emoji).
The expanded view lists doc titles with their reader URLs.

## Server

### Storage

Add tables to the retained queue DB, where Codex review registrations live
(`sm_send.db_path` / `RetainedQueueStore`). Follow that module's migration pattern.

```sql
CREATE TABLE owner_docs (
  id TEXT PRIMARY KEY,               -- 8 hex chars, like other sm ids
  repo TEXT NOT NULL,                -- owner/name
  path TEXT NOT NULL,                -- repo-relative
  pr_number INTEGER,                 -- NULL for commit-only docs
  author_session_id TEXT NOT NULL,
  author_session_name TEXT,          -- snapshot so retired sessions still show a name
  title TEXT NOT NULL,
  note TEXT,
  review_requested INTEGER NOT NULL DEFAULT 0,
  retracted_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX owner_docs_key ON owner_docs(repo, path, IFNULL(pr_number, -1));

CREATE TABLE owner_doc_publishes (   -- one row per `sm doc publish`
  id INTEGER PRIMARY KEY,
  doc_id TEXT NOT NULL REFERENCES owner_docs(id),
  commit_sha TEXT NOT NULL,
  blob_sha TEXT NOT NULL,            -- from the contents API; detects whether the file really changed
  session_id TEXT NOT NULL,
  published_at TEXT NOT NULL
);

CREATE TABLE owner_doc_views (       -- drives unread/updated
  doc_id TEXT NOT NULL,
  blob_sha TEXT NOT NULL,
  viewed_at TEXT NOT NULL,
  PRIMARY KEY (doc_id, blob_sha)
);

CREATE TABLE owner_doc_drafts (      -- server-side so a draft started on the phone continues on the laptop
  id TEXT PRIMARY KEY,
  doc_id TEXT NOT NULL,
  commit_sha TEXT NOT NULL,          -- the revision the comment was written against
  line INTEGER,                      -- source line in the file at commit_sha; NULL = not placeable
  quote TEXT NOT NULL,               -- the selected text
  body TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE owner_doc_reviews (
  id TEXT PRIMARY KEY,
  doc_id TEXT NOT NULL,
  commit_sha TEXT NOT NULL,
  verdict TEXT NOT NULL,             -- approve | changes_requested | comment
  body TEXT,
  comment_count INTEGER NOT NULL,
  github_review_id INTEGER,
  github_review_url TEXT,
  submitted_at TEXT NOT NULL,
  delivered_to_session_id TEXT
);
```

Derived doc state:

- `review_requested`, with no review at a blob SHA ≥ the latest publish → **Review requested**
- A review exists for the latest blob → **Reviewed**
- The latest blob has been viewed → **Read**
- An older blob has been viewed, the latest hasn't → **Updated**
- Otherwise → **New**

Compare **blob** SHAs, not commit SHAs, so pushes that don't touch the file don't
mark it updated.

### Fetching content

- `gh api -H "Accept: application/vnd.github.raw" repos/{repo}/contents/{path}?ref={sha}`,
  run through the existing `gh` command helper in `http.rs`. Use the non-raw JSON
  variant when the blob SHA is also needed (at publish time).
- Cache on disk at `<state dir>/doc_cache/{repo}/{commit_sha}/{path}`. Content at a
  SHA never changes, so the cache is never invalidated; prune entries unused for 30 days.
- The latest revision of a PR doc is the PR head (`gh pr view` or
  `repos/{repo}/pulls/{n}`), cached for 30s. The head SHA stays readable after merge
  or branch deletion (GitHub keeps `refs/pull/N/head`). PR state
  (`open`/`closed`/`merged`) comes from the same call and decides whether commenting
  is enabled.
- For a PR doc, "latest" means the PR head, even when the agent hasn't republished.
  The owner sees what's actually in the PR. Publish events still mark
  the revisions the agent explicitly announced.
- Never call `gh` from the list projection. Lists use stored data only; `gh` calls
  happen on view, publish and submit.

### Rendering (`GET /docs/{id}/view?sha=<sha>`)

1. Fetch the file at `sha`.
2. Choose the renderer by extension:
   - `.html`/`.htm`: serve as-is, plus the line annotation below.
   - `.md`: render with `pulldown-cmark` using `into_offset_iter` to get source
     offsets, and wrap in a minimal readable HTML shell.
   - Anything else: HTML-escape it inside `<pre>`.
3. **Line annotation**: add `data-sm-line="N"` (1-based line of the start tag in the
   source) to block-level start tags: `p li h1-h6 pre blockquote td th dt dd figcaption
   div section tr`. For HTML, do a single forward scan that tracks line numbers and
   skips `<script>`, `<style>`, `<!-- -->` and attribute values. It edits the start
   tag in place and doesn't reserialize the document, so the owner's HTML is otherwise
   untouched byte for byte. For markdown, add the attribute from the source offsets
   during rendering.
4. **Inject the review client** (one `<script>` plus one `<style>`, just before
   `</body>`, or appended if there's none) with this config inlined:
   `{docId, sha, latestSha, prNumber, prState, canComment, token, drafts[]}`.
5. Record a view of this blob SHA.

`token` is an HMAC over `doc_id|sha|expiry` (24h), signed with the server's existing
session-cookie secret. The doc endpoints below accept either normal auth or
`X-SM-Doc-Token` scoped to that doc. That way the page's own `fetch` calls work in the
Android WebView, where device auth headers only accompany the initial `loadUrl`.

### Review client (the injected script)

Plain JS, no dependencies, under ~20 KB. It must not break the doc's own scripts
(for example the memo dark-mode toggle): namespace everything under `window.__smDoc`
and use a shadow DOM for its UI.

- **Header bar** (fixed, collapsible): title, `sha7`, revision picker (the publish
  events plus the PR head), PR link, state. If `canComment` is false, it reads
  "Read-only: PR closed / no PR".
- **Newer-revision banner**: poll `GET /docs/{id}/head` every 60s. If the latest SHA
  differs from the viewed one, show "A newer revision was pushed. Load it."
- **Selecting what to comment on**:
  - Desktop: select text → a "Comment" chip appears next to the selection.
  - Touch: tap a block with `data-sm-line` → it highlights and the chip appears (text
    selection in a mobile WebView is too fiddly). Long-press still allows normal text
    selection for a narrower quote.
  - Anchor: `line` = the `data-sm-line` of the closest ancestor with one;
    `quote` = the selected text, or the block's `textContent` (trimmed, capped at 300
    chars) on a tap.
- **Composer**: textarea → `POST /docs/{id}/drafts`. Drafts show as margin markers
  (desktop) or inline badges (mobile) on their block; tap to edit or delete.
- **Submit panel**: a "Review (N)" button opens verdict radios (Approve / Request
  changes / Comment), an overall body textarea and Submit →
  `POST /docs/{id}/review`. On success it shows the GitHub review link and clears the
  drafts.
- **Drafts from another revision**: if drafts exist for a different SHA than the one
  being viewed, show "N draft comments on <sha7>", with actions *Submit them against
  <sha7>* (switches the view to that SHA) or *Discard*. Don't auto-migrate drafts
  between revisions.

### Submitting a review (`POST /docs/{id}/review`)

Body: `{sha, verdict, body}`. Uses the stored drafts for `(doc_id, sha)`.

1. Refuse unless the doc has a PR and the PR is open.
2. Build one GitHub review: `POST repos/{repo}/pulls/{n}/reviews` with
   `commit_id = sha` and `event = "COMMENT"`. Each draft with a line becomes
   `{path, line, side: "RIGHT", body: "> <quote>\n\n<comment>"}`. Quote the selected
   text in every comment: the line anchor is a block start, which may cover a long
   paragraph.
3. **Verdict in the body, not the event.** Agents open PRs through the owner's `gh`
   account, so the owner is the PR author, and GitHub rejects `APPROVE` and
   `REQUEST_CHANGES` on your own PR. Always send `event: COMMENT`. The body starts
   with `**Verdict: Approved**`, `**Verdict: Changes requested**` or
   `**Verdict: Comments**`, followed by the owner's overall text. The wake message
   carries the verdict as well.
4. Drafts with `line = NULL`, or any comment GitHub rejects with 422, go into the
   review body under "Comments not anchored to a line" as quote + comment. Try the
   full review first. On a 422 that names specific comments, move those into the
   body and retry once. On an unspecific 422, move all of them into the body and retry.
5. Store the `owner_doc_reviews` row and delete the drafts.
6. **Wake the author** through the normal durable `sm send` path:
   ```
   [sm review] Rajesh's review of "<title>" (PR #<n> @ <sha7>) is here: <review_url>
   Verdict: changes requested · 5 inline comments · 1 unanchored
   ```
   Recipient: the author session if it exists (stopped sessions get the queued
   message on restore, as with other sends). If the author has been retired, send to
   its parent if there is one. Otherwise, leave `delivered_to_session_id` NULL and
   show "Review not delivered: author retired" in the reading list. Don't fail the
   submit.

### Obligations projection

In `project_session_obligations`, add the following for each author session:

- `docs: [{id, title, state, repo, path, pr_number, latest_commit_sha, published_at}]`
- When `state == review_requested`, also add a `waiting_on` entry
  `{kind: "owner_review", id, label: "Owner review · <title>", since: <publish time>}`.
  The agent then shows as "Waiting for owner review", which is accurate: it's blocked
  on the owner.

Bump `schema_version`. Android `ApiModels.kt` gets `docs` as an optional field
defaulting to empty. Older app builds must keep parsing the response; follow the
existing optional-field pattern.

### Endpoints

| Method | Path | Purpose |
|---|---|---|
| POST | `/docs` | publish (from the CLI; session auth like other CLI writes) |
| GET | `/docs?session=<id>` | JSON list for the CLI (a session and, optionally, its descendants) |
| GET | `/docs/{id}` | redirect to the view at the latest SHA (JSON metadata with `?format=json`) |
| GET | `/docs/{id}/view?sha=` | rendered doc plus the review client |
| GET | `/docs/{id}/raw?sha=` | raw file (download, debugging) |
| GET | `/docs/{id}/head` | `{latest_sha, pr_state}` for the banner poll |
| POST/PATCH/DELETE | `/docs/{id}/drafts[/{draft_id}]` | draft CRUD |
| POST | `/docs/{id}/review` | submit the review |
| POST | `/docs/{id}/retract` | hide |

All doc routes go through `ensure_session_read_allowed` / `ensure_core_writes_enabled`
as appropriate, plus the doc-token alternative described above. Add the routes to the
read-only HTTP test inventory (`tests/read_only_http.rs`) where applicable.

## Phases

**Phase 0 — verify GitHub behaviour (do this before writing the review code).**
On a scratch PR in this repo, confirm via the API:
(a) a review comment on a line **outside** the diff hunks of a modified file is
accepted with `line` + `side: RIGHT` + `commit_id`. The web UI allows this after
expanding the file; confirm the API does too.
(b) `APPROVE` on your own PR is rejected.
(c) a review with `commit_id` older than the head is accepted.
Record the results in this spec. If (a) turns out false, the 422 fallback in the
submit step already handles it.

**Phase 1 — publish and read.** Tables, `sm doc publish/list/show/retract`,
fetch + cache, view rendering (without the review client), obligations projection,
Android Docs surface + reading list + reader, web `/docs`, `sm watch` marker,
AGENTS.md section. This alone solves "I can't find or read the doc".

**Phase 2 — review.** Line annotation, review client, drafts, submit, wake.

**Later (not in scope).** Highlight what changed between the last reviewed SHA and
the current one, computed from the two blobs at render time. Show existing GitHub
review threads inline in the reader.

## Tests

Use `./scripts/test-rust-isolated.sh`. Stub `gh` the same way the Codex review watch
tests do (reuse that fixture pattern; don't invent a new one).

- CLI resolution: with `--pr` (head mismatch warning; file missing from the PR),
  without flags (dirty file → error; unpushed HEAD → error), repo slug parsing for
  SSH and HTTPS remotes.
- Republishing the same key adds a publish event, not a new doc. A different PR for
  the same path is a different doc.
- State derivation: new → read → updated only when the blob SHA changes, not on
  unrelated pushes; review requested → reviewed.
- Line annotation: correct line numbers; tags inside `<script>`/`<style>`/comments
  untouched; attribute values containing `>` or newlines; output identical to the
  input apart from the inserted attributes. Snapshot-test against a real memo
  (copy `~/artifacts/1612-context/1612_decision_memo.html` into test fixtures).
- Markdown line mapping.
- Submit: payload shape (`event: COMMENT`, verdict header, quotes, `commit_id`); 422
  fallback moves comments into the body; closed PR is refused; wake message
  text and recipient routing (live author, retired author → parent, none → undelivered).
- Doc token: accepted for its own doc, rejected for another doc or when expired.
- Projection: `docs` and the `owner_review` waiting entry; old-schema Android
  parsing still works.
- Manual: publish a real memo from a worktree, open it on the phone over the
  Cloudflare hostname, leave two tap comments, submit, confirm the GitHub review and
  the agent's wake.

## Non-goals

- Copying or versioning doc bytes in sm.
- Sandboxing agent HTML.
- Replying to or resolving GitHub threads from sm (the agent does that with `gh`; the
  owner can use GitHub).
- Docs outside GitHub-hosted repos.
