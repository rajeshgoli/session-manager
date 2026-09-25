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
  Read the review with `gh`, address the comments, push, and run
  `sm doc publish <path> --pr <N>` again (add `--review` if you want another review)
  so the owner sees the new revision. Always republish with the same `--pr`; a
  publish without it is a different, read-only doc.
- Prefer self-contained HTML (inline CSS, images as data URIs). Markdown also works.
```

## User-facing surfaces

### CLI

```bash
sm doc publish <path> [--pr N | --commit SHA | --no-pr] [--title "..."] [--note "..."] [--review]
sm doc list [--session <id>] [--json]          # default: the caller's session tree
sm doc show <doc> [--json]                      # metadata, revisions, reviews, URLs
sm doc retract <doc>                            # hide from the session Docs row (does not touch git)
```

`<doc>` is `<repo-name>/<path in repo>` (for example
`fractal-algo-rust/docs/working/ticket-title.html`) or the doc's URL pasted as
printed; a pasted URL's `?version=` picks the same doc the link opens. Output names
docs the same way and never prints the internal doc id, including `--json` (the
`id` field and each publish's `doc_id` are dropped).

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
   - Neither: if the current branch has an open PR (`gh pr view --json number,state`
     with no argument), behave as `--pr <that number>` and print
     `Using PR #N for the current branch (pass --no-pr for a commit-only doc)`.
     This keeps a republish on the same doc even when the agent forgets `--pr`.
   - `--no-pr`, or no open PR for the branch: use `HEAD`. Fail with "commit and push
     first" if the file has uncommitted changes, or if `HEAD` isn't on any
     remote-tracking branch (`git branch -r --contains HEAD` is empty).
3. `--review` requires `--pr` and an open PR.
4. POST to the server with the author session from `CLAUDE_SESSION_MANAGER_ID`.
5. Print `Published "<title>" (<repo-name>/<path>) at <sha7> → <reader URL>`.

Identity and republishing: a doc is keyed by `(repo, path, pr_number)`, or by
`(repo, path)` when there's no PR. Publishing the same key again adds a new
**publish event** to the existing doc; it doesn't create a second doc. The latest
publish carries the current title and note; each publish row records whether that
publish requested a review.

Authorship follows the agent working the doc. A republish by a different session
keeps the recorded author while that author is live, so one agent can't take
another's reviews. If the recorded author is retired or killed, or its session no
longer exists, the republishing session becomes the author (`author_session_id` and
`author_session_name`). Example: `1471-engineer` publishes the memo and is retired;
`1452-spec-author` republishes it, and from then on the memo shows in
`1452-spec-author`'s Docs row and the owner's review wakes `1452-spec-author`.

### Android app

- **Session expansion** (`AgentWorkSections` in `WatchScreen.kt`): add a **Docs**
  surface before "Reviews". One row per doc authored by the session: title, state chip
  (`New` / `Updated` / `Review requested` / `Reviewed` / `Read`) and relative publish
  time. Tapping opens the reader.
- No global reading list here. Docs attach to the session that wrote them. A
  cross-cutting view of which agent worked on which ticket, opened which PR, and wrote
  which doc is the history page in the separate ticket/PR-claims work (see "Related
  work").
- **Reader**: a full-screen WebView loading the doc's readable URL (`reader_path`,
  see "Doc URLs") with the same auth the
  app uses for API calls (pass the device auth headers on `loadUrl`). WebView doesn't
  resend custom headers when the page navigates itself (revision picker, newer-revision
  banner, cross-revision drafts). So override `shouldOverrideUrlLoading`: for any
  same-origin URL under `/docs/`, cancel the navigation and call
  `loadUrl(url, deviceAuthHeaders)`. Other URLs (the PR link, external links in the doc)
  open in the system browser. All review UI lives inside the served page (below), so
  Android needs no native comment UI; one implementation serves every client. Enable
  JavaScript and DOM storage, and handle back navigation.

### Web (laptop / studio browser)

The reader is a plain server page behind the existing browser-session (Google) auth,
or the owner's Cloudflare Access browser login (#1463), at the readable URL below, so
any doc link works in a laptop browser. The Rust server serves no web session list today (`web/sm-watch` is
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
  review_requested INTEGER NOT NULL DEFAULT 0,  -- this publish asked for a review (--review)
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
  id TEXT PRIMARY KEY,               -- the client's submission_id (see "Submitting a review")
  status TEXT NOT NULL,              -- submitting | posted | failed
  pending_review_node_id TEXT,       -- GraphQL id of the pending review while submitting
  doc_id TEXT NOT NULL,
  commit_sha TEXT NOT NULL,
  blob_sha TEXT NOT NULL,            -- blob reviewed, for state derivation
  verdict TEXT NOT NULL,             -- approve | changes_requested | comment
  body TEXT,
  line_comment_count INTEGER NOT NULL,   -- posted as LINE threads
  file_comment_count INTEGER NOT NULL,   -- posted as FILE threads (no line, or line fallback)
  github_review_id INTEGER,
  github_review_url TEXT,
  submitted_at TEXT NOT NULL,
  delivered_to_session_id TEXT
);
```

Derived doc state, first match wins. The *latest blob* is the `blob_sha` of the
latest publish row. The projection uses stored data only (see "Fetching content"), so
it never looks at the live PR head.

1. The latest publish has `review_requested`, and no **posted** review was submitted
   after that publish's `published_at` → **Review requested**. A review request
   belongs to a publish event, so republishing an already-reviewed blob with
   `--review` asks again.
2. A posted review exists whose `blob_sha` equals the latest blob → **Reviewed**
3. A view exists for the latest blob → **Read**
4. A view exists for some other blob → **Updated**
5. Otherwise → **New**

Compare **blob** SHAs, not commit SHAs, so pushes that don't touch the file don't
mark it updated. Blob SHAs are equality-compared only; they have no order.

The server computes a blob SHA from fetched content itself, as git does:
`sha1("blob <byte length>\0" + bytes)`. That is how `/view` records a view and a
review records its blob without an extra API call. At publish time, check the
computed value against the contents API's `sha` once; a test pins that the two
match.

### Fetching content

- `gh api -H "Accept: application/vnd.github.raw" repos/{repo}/contents/{path}?ref={sha}`,
  run through the existing `gh` command helper in `http.rs`. Use the non-raw JSON
  variant when the blob SHA is also needed (at publish time).
- Cache on disk at `<state dir>/doc_cache/{repo}/{commit_sha}/{path}`. Content at a
  SHA never changes, so the cache is never invalidated; prune entries unused for 30 days.
- The PR head SHA and state come from `gh pr view` (or `repos/{repo}/pulls/{n}`),
  cached for 30s. The head SHA stays readable after merge
  or branch deletion (GitHub keeps `refs/pull/N/head`). PR state
  (`open`/`closed`/`merged`) comes from the same call and decides whether commenting
  is enabled.
- The reader opens the **latest published** SHA by default, the revision the agent
  announced as ready. That's also the revision the state chip describes, so the chip
  and what you open always agree. If the PR head's file blob differs (the agent pushed
  without republishing), the reader shows a banner, "The PR has newer unpublished
  changes to this doc", with a link to view the head. You can read and review the head
  too; it's simply not the default.
- Never call `gh` from the list projection. Lists use stored data only; `gh` calls
  happen on view, publish and submit.

### Doc URLs

Every doc URL a person or agent sees has one form (#1465):

```
https://<browser host>/docs/<repo-name>/<path in repo>?version=<commit SHA prefix>
https://sm.rajeshgo.li/docs/fractal-algo-rust/docs/working/ticket-title.html?version=856f0d6e1a2b
```

- `<repo-name>` is the GitHub repo name without the owner (all repos belong to the
  owner); it matches case-insensitively. `<path in repo>` is the stored path, each
  segment percent-encoded (`notes/my memo#1.md` → `notes/my%20memo%231.md`).
- `?version=` is a commit SHA prefix, 7 to 40 hex characters, any case, that must
  match one of the doc's publishes. Printed links carry 12 characters of the latest
  publish's commit. Without `?version=` the page renders the latest publish. A
  version that matches no publish may name a PR doc's current PR head; that is the
  link the unpublished-changes banner and the revision picker use to show changes
  the agent pushed without republishing. Once the head moves on, the link is a 404
  like any other unknown version.
- The page renders in place. It never redirects, so the address bar keeps the
  readable URL.
- One path can hold two docs, a PR doc and a commit-only doc. Without a version, the
  newest publish across both renders; with one, the newest publish whose commit
  matches renders.
- 404 when no doc has that repo name and path (`Doc not found`), when no publish
  matches the version or the version is not 7 to 40 hex characters
  (`Version not found`), or when the prefix matches two different commits
  (`Version matches more than one commit; use more characters`). Retracted docs
  still resolve, as they do by id.
- `?format=json` returns the same metadata as `GET /docs/{id}?format=json`; this is
  how `sm doc show/retract` resolve a name.
- Auth is the same as the id read routes: the SM Google session, the owner's
  Cloudflare Access browser login on the browser host, or the studio's local bypass.

| Example | Result |
|---|---|
| `/docs/widgets/specs/memo.md` | latest publish |
| `/docs/widgets/specs/memo.md?version=aaaaaaaaaaaa` | the publish at `aaaaaaaaaaaa…` |
| `/docs/Widgets/specs/memo.md?version=AAAAAAA` | same publish (7 characters, any case) |
| `/docs/widgets/specs/memo.md?version=bbbbbbbbbbbb` | 404 `Version not found` |
| `/docs/gadgets/specs/memo.md` | 404 `Doc not found` |
| `/docs/widgets/view` | the doc at path `view`, not an id route |

The id routes (`/docs/{id}`, `/view`, `/raw`, `/head`, `/drafts`, `/review`,
`/retract`) are internal API and never appear on the web or in the app. Every URL the
server hands out uses the readable form: `reader_path`, `reader_url` and
`browser_url` in `GET /docs`, `GET /docs/{id}?format=json` and
`/session-obligations`; `sm doc` and `sm watch` output; links the Android reader and
the review client show or share. `GET /docs/{id}` redirects to the readable URL of
the latest publish. Each doc projection also carries `name`
(`<repo-name>/<path in repo>`).

A GET under `/docs/<a>/<rest>` is an id route only when `<a>` is a stored doc id and
`<rest>` is `view`, `raw`, `head` or `drafts`; everything else is a readable name.
POST, PATCH and DELETE under a doc are id-only (`/retract`, `/drafts`, `/review`,
`/drafts/{draft_id}`); one to a readable name is a 404.

### Rendering

1. Fetch the file at `sha`.
2. Choose the renderer by extension:
   - `.html`/`.htm`: serve as-is, plus the line annotation below.
   - `.md`: render with `pulldown-cmark` using `into_offset_iter` to get source
     offsets, and wrap in a minimal readable HTML shell.
   - Anything else: HTML-escape it inside `<pre>`.
3. **Line annotation**: add `data-sm-line="N"` (1-based line of the start tag in the
   source) to block-level start tags: `p li h1-h6 pre blockquote td th dt dd figcaption
   div section tr`. For HTML, do a single forward scan that tracks line numbers and
   skips comments (`<!-- -->`), attribute values, and the contents of raw-text and
   RCDATA elements, up to their matching end tag: `script style textarea title xmp
   iframe noembed noframes noscript plaintext`. Tag-like text inside those, such as
   `<textarea><p>example</p></textarea>`, is content, not markup. It edits the start
   tag in place and doesn't reserialize the document, so the owner's HTML is otherwise
   untouched byte for byte. For markdown, add the attribute from the source offsets
   during rendering.
4. **Inject the review client** (one `<script>` plus one `<style>`, just before
   the first real `</body>` end tag, or appended if there's none) with this config
   inlined: `{docId, title, name, sha, latestSha, prNumber, prState, prUrl,
   canComment, token, revisions[], drafts[], unfinishedReview}`. `revisions` lists the publishes
   newest first (`sha`, `blobSha`, `publishedAt`, `reviewRequested`, readable
   `path`); `drafts` holds every draft on the doc, across revisions. `prState` is
   `open`/`closed`/`merged`, `unknown` when `gh` fails (commenting then stays off
   until `/head` answers), or null for a commit-only doc. `<` in the inlined JSON
   is escaped so the config can't close the script.
5. Record a view of this blob SHA.

`token` is an HMAC over `doc_id|expiry` (24h), signed with the server's existing
session-cookie secret, sent as `smdt_<doc_id>.<expiry unix seconds>.<base64url
HMAC-SHA256>`; it is null when the server has no session-cookie secret. It covers the whole doc, not one SHA, so it stays valid across
revisions. The doc's JSON endpoints (`/head`, `/drafts`, `/review`) accept either
normal auth or an `X-SM-Doc-Token` header for that doc. That way the page's own
`fetch` calls work in the Android WebView, where device auth headers don't accompany
script requests. Page navigations (`/view`) always use normal auth; on Android the
reader's navigation override re-sends it. Tokens never go in URLs.

### Review client (the injected script)

Plain JS, no dependencies, under ~20 KB. It must not break the doc's own scripts
(for example the memo dark-mode toggle): namespace everything under `window.__smDoc`
and use a shadow DOM for its UI.

- **Header bar** (fixed, collapsible): title, `sha7`, revision picker (the publish
  events, plus the PR head when its blob differs from every published one), PR link,
  state. If `canComment` is false, it reads
  "Read-only: PR closed / no PR".
- **Newer-revision banner**: poll `GET /docs/{id}/head?sha=<viewed sha>` on load and
  every 60s while the page is visible. It returns `{latest_published_sha,
  latest_reader_path, pr_head_sha, pr_head_blob_sha, pr_head_blob_differs,
  pr_head_reader_path, pr_state}`; `pr_head_blob_differs` compares the file at the
  PR head with the viewed revision (the latest publish without `?sha=`), and is
  false when the file doesn't exist at the head. If a newer revision has been
  **published** (after the page loaded, or the viewed publish is not the latest),
  show "A newer revision was published. Load it." Otherwise, if the PR head's blob
  differs from the viewed one, show "The PR has newer unpublished changes to this
  doc" with a link to the head. The poll also refreshes `prState` and whether
  commenting is on.
- **Selecting what to comment on**:
  - Desktop: select text → a "Comment" chip appears next to the selection.
  - Touch: tap a block with `data-sm-line` → it highlights and the chip appears (text
    selection in a mobile WebView is too fiddly). Long-press still allows normal text
    selection for a narrower quote.
  - Anchor: `line` = the `data-sm-line` of the closest ancestor with one;
    `quote` = the selected text, or the block's `textContent` (trimmed, capped at 300
    chars) on a tap.
- **Composer**: textarea → `POST /docs/{id}/drafts`. A revision holds at most 100
  drafts (a 400 beyond that), the page size reconciliation reads back from GitHub. Drafts show as margin markers
  (desktop) or inline badges (mobile) on their block; tap to edit or delete.
- **Submit panel**: a "Review (N)" button opens the drafts for this revision (each
  with Edit), verdict radios (Approve / Request changes / Comment), an overall body
  textarea and Submit → `POST /docs/{id}/review`. On success it shows the GitHub
  review link and clears the drafts. The server posts every stored draft for the
  revision, so the panel reloads the drafts when it opens and again right before
  submitting; if they changed (another tab or device), it shows the new list and
  asks the owner to submit again. The panel keeps one `submission_id`, with the
  verdict and body of that first attempt, until the review is posted: every retry,
  after any error, reuses all three (the verdict and body show read-only), and the
  server reconciles it against GitHub, so resubmitting never posts twice.
- **Drafts from another revision**: if drafts exist for a different SHA than the one
  being viewed, show "N draft comments on <sha7>", with actions *Submit them against
  <sha7>* (switches the view to that SHA) or *Discard*. Discard drops only the
  drafts the server deleted and says how many could not be discarded. Don't auto-migrate drafts
  between revisions.

### Submitting a review (`POST /docs/{id}/review`)

Body: `{submission_id, sha, verdict, body}`. Uses the stored drafts for
`(doc_id, sha)`. The review client generates `submission_id` (a UUID) when the submit
panel opens and reuses it on every retry of that submit.

**Idempotency.** Before any GitHub call, insert the `owner_doc_reviews` row with
`id = submission_id` and `status = submitting`. If the row already exists:
`posted` → return the stored result without touching GitHub; `failed` or
`submitting` → set it `submitting` and reconcile (below) instead of starting over.
A `submission_id` from another doc is a 409. A new `submission_id` for a revision
that still has a `submitting` row resumes that row instead (a reloaded page has
lost its id), and the page config carries it as `unfinishedReview: {id, verdict,
body}` so the panel shows what will be posted. Until that row resolves, draft
writes for its revision are a 409. Submits, reconciliation and draft writes are
serialized. The review body ends with a hidden
marker, `<!-- sm-review:<submission_id> -->`. To reconcile, list the PR's reviews by
the viewer (GraphQL `pullRequest.reviews(author: <viewer login>)`, including
`PENDING`, every page) and look for the marker:
- a submitted review carries it → finish steps 5–6 from that review, counting its
  comments with a line as line comments and the rest as file comments;
- a pending review carries it → add the threads of drafts whose quoted body isn't
  on it yet (steps 2.2–2.3), submit it (step 2.4), then finish;
- neither → delete any pending review recorded in `pending_review_node_id`, then
  run the flow again.

The row changes from `submitting` to `posted` in the same transaction that deletes
the drafts and enqueues the wake, so the wake fires exactly once. A server restart
reconciles any rows left in `submitting`. The GitHub
behaviour each step relies on was verified on PR #1448; see "GitHub API findings".

1. Refuse unless the doc has a PR and the PR is open.
2. **Use GraphQL's pending-review flow, not a single REST call.** One bad inline
   comment makes GitHub reject the whole REST review with a generic
   `Line could not be resolved` that doesn't say which comment failed (F5). The
   pending flow lets each comment fall back on its own:
   1. `addPullRequestReview(input: {pullRequestId, commitOID: <sha>, body})` with no
      event creates a pending review pinned to the viewed SHA. The body includes the
      marker. Record its id in `pending_review_node_id` right away.
   2. For each draft that has a line, run
      `addPullRequestReviewThread(input: {pullRequestReviewId, path, line, side: RIGHT, subjectType: LINE, body})`.
      **A `null` thread with no error means it failed** (F2). Treat it exactly like
      an error.
   3. For each draft that failed, or has `line = NULL`, run
      `addPullRequestReviewThread(... subjectType: FILE, body)`. This is a file-level
      comment on the doc, and it works inside a review (F3).
   4. `submitPullRequestReview(input: {pullRequestReviewId, event: COMMENT, body})`,
      with the same body as step 1, marker included.
   5. If GitHub returns an error after step 1, first check the viewer's reviews for a
      *submitted* one carrying the marker (a lost response to step 4) and finish from
      it if found. Otherwise delete the pending review (`deletePullRequestReview`) so
      no half-built pending review is left under the owner's account (a leftover
      pending review blocks creating the next one). Then mark the row `failed` and
      keep the drafts. If the delete itself fails, the row stays `submitting`, so
      a retry resumes that pending review. GraphQL mutations are not retried on
      transport errors, since a retry after a lost response could add a second
      thread; instead an error is treated as possibly applied:
      - step 1 failing → look for a review carrying the marker; use it if found,
        mark the row `failed` if GitHub confirms there is none, and leave it
        `submitting` if GitHub can't be asked;
      - a `LINE` thread erroring → check whether the pending review already holds
        that comment before falling back to `FILE`. The client retries with a new
      `submission_id`. A crash, as opposed to an error, leaves the row `submitting`,
      and reconciliation handles it.
3. **Every comment body quotes the selected text**, as `> <quote>\n\n<comment>`. A
   line anchor is the start of a block, which may be a long paragraph, and a
   file-level comment has no anchor at all. The quote is what tells the agent where
   the comment applies.
4. **The verdict goes in the body; the event is always `COMMENT`.** Agents open PRs
   through the owner's `gh` account, so the owner is the PR author, and GitHub
   rejects both `APPROVE` and `REQUEST_CHANGES` on your own PR (F4). The body starts
   with `**Verdict: Approved**`, `**Verdict: Changes requested**` or
   `**Verdict: Comments**`, followed by the owner's overall text. The wake message
   carries the verdict too.
5. In one transaction: set the row to `posted` with `line_comment_count`,
   `file_comment_count` (as actually posted, after fallbacks) and the GitHub review
   id and URL; delete the drafts; enqueue the wake (step 6).
6. **Wake the author** through the normal durable `sm send` path:
   ```
   [sm review] Rajesh's review of "<title>" (PR #<n> @ <sha7>) is here: <review_url>
   Verdict: changes requested · 5 line comments · 1 file comment
   ```
   Recipient: the author session if it exists (stopped sessions get the queued
   message on restore, as with other sends). If the author has been retired, send to
   its parent if there is one and it isn't retired too. Otherwise, leave
   `delivered_to_session_id` NULL and show "Review not delivered: author retired" on
   the doc row (Android, `sm watch`) and in `sm doc show`; doc projections carry
   `review_undelivered` for the latest posted review. Don't fail the submit.
   Zero counts are left out of the second line ("no comments" when both are zero).

When does a comment fall back to file level? Only when the file is **modified** by
the PR (it existed on the base branch) and the selected block is outside the diff
hunks. A doc **added** by the PR, the normal case for memos, is entirely inside the
diff at every revision, because the diff is always base...head. Every line can take
an inline comment.

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
| GET | `/docs/<repo-name>/<path>?version=` | readable reader: the rendered doc (JSON metadata with `?format=json`); see "Doc URLs" |
| GET | `/docs/{id}` | redirect to the readable URL of the latest publish (JSON metadata with `?format=json`, including `publishes` and `reviews`) |
| GET | `/docs/{id}/view?sha=` | rendered doc plus the review client |
| GET | `/docs/{id}/raw?sha=` | raw file (download, debugging) |
| GET | `/docs/{id}/head?sha=` | `{latest_published_sha, latest_reader_path, pr_head_sha, pr_head_blob_sha, pr_head_blob_differs, pr_head_reader_path, pr_state}` for the banners |
| GET/POST | `/docs/{id}/drafts` | list the doc's drafts / create one: `{sha, line, quote, body}` |
| PATCH/DELETE | `/docs/{id}/drafts/{draft_id}` | edit a draft's `body` / delete it |
| POST | `/docs/{id}/review` | submit the review |
| POST | `/docs/{id}/retract` | hide |

All doc routes go through `ensure_session_read_allowed` / `ensure_core_writes_enabled`
as appropriate, plus the doc-token alternative described above. Add the routes to the
read-only HTTP test inventory (`tests/read_only_http.rs`) where applicable.

## GitHub API findings (verified on PR #1448, 2026-09-24)

Probe reviews on #1448 are labelled `[probe ...]`. The PR briefly carried a one-line
AGENTS.md edit to test a modified file; it was reverted in the next commit.

| # | Probe | Result |
|---|---|---|
| F1 | Inline comment on a line **outside** the diff hunks of a modified file (REST `pulls/{n}/reviews`, REST `pulls/{n}/comments`, GraphQL `addPullRequestReview` with `threads`) | **Rejected** by all three: `Line could not be resolved`. The web UI may allow it after expanding the file, but the public API does not. |
| F2 | Same, via GraphQL pending review + `addPullRequestReviewThread(subjectType: LINE)` | Returns `thread: null` with **no error**, so the call looks like success |
| F3 | File-level thread (`subjectType: FILE`) inside a pending review, then submit | **Works**; the comment has `line: null` |
| F4 | `APPROVE` / `REQUEST_CHANGES` on your own PR | **Rejected**: `Can not approve your own pull request` / `Can not request changes on your own pull request` |
| F5 | REST review with one valid and one invalid inline comment | **Whole review rejected**, with an error that doesn't identify the bad comment |
| F6 | Review with `commit_id` older than the PR head | **Accepted**, pinned to that commit |
| F7 | Inline comment against an older commit on a file whose change was later reverted (it's no longer in the head diff) | **Accepted**, so line resolution uses the diff at `commit_id`, not at head |
| F8 | Control: inline comment inside a hunk | Accepted |

What this means for the design: F6 and F7 confirm that pinning a review to the viewed
SHA is sound. F1–F3 and F5 lead to the per-comment fallback to a file-level comment
in the submit steps. F4 leads to the verdict going in the body.

Anchoring check: in `1612_decision_memo.html` (536 lines, 31 `<p>`), every block
starts on its own source line, so the line anchor from `data-sm-line` is exact for
hand-written memos. Generated or minified HTML can pack many blocks onto one line
(`walkthrough.html`: 8 lines, 7 `<p>`). The anchor is then coarse, and the quoted
text in each comment disambiguates.

## Tickets

Epic #1447. Each ticket fits within one agent's context.

- **#1449, server + CLI + web reader.** Storage tables; `sm doc publish/list/show/retract`
  (no `--review` yet); content fetch and cache; `GET /docs/{id}`, `/view` (rendering
  without the review client or line annotation), `/raw`; the `docs` field in the
  obligations projection; the `sm watch` marker; the read-only part of the AGENTS.md
  "Docs for the owner" section. On its own, this solves "I can't find or read the
  doc" on a laptop.
- **#1450, Android.** The Docs surface in `AgentWorkSections`, the optional `docs`
  field in `ApiModels.kt`, and the WebView reader with device auth, including the
  `shouldOverrideUrlLoading` re-auth for `/docs/` navigations. Depends on #1449.
  This solves it on mobile.
- **#1451, review.** Line annotation; the injected review client; drafts; the doc
  token; `/head`; submit using the GraphQL flow above; the `[sm review]` wake; the
  `owner_review` waiting entry; `--review`; the review part of the AGENTS.md section.
  Depends on #1449 and #1450.

Out of scope, possible later tickets: highlight what changed between the last
reviewed SHA and the current one, computed from the two blobs at render time; show
existing GitHub review threads inline in the reader.

## Related work

- **#1452, `sm ticket` / `sm pr` claims and an agent history page.** Agents claim the
  tickets and PRs they work on; sm shows which agent worked on which ticket, opened
  which PR, and wrote which docs, and detects two agents on the same ticket or PR.
  This replaces a global doc reading list: cross-session doc discovery belongs on
  that history page, cut by agent, ticket or PR. Owner docs don't depend on it. When
  #1452 lands, a doc's PR links it to the claim records automatically.

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
- Line annotation: correct line numbers; tag-like text inside comments and every
  raw-text/RCDATA element (`<script>`, `<style>`, `<textarea>`, `<title>`, ...)
  untouched; attribute values containing `>` or newlines; output identical to the
  input apart from the inserted attributes. Snapshot-test against a real memo
  (copy `~/artifacts/1612-context/1612_decision_memo.html` into test fixtures).
- Markdown line mapping.
- Submit (stub GraphQL): pending review pinned to `commitOID`; a `null` thread and an
  error both fall back to a `FILE` thread; the event is always `COMMENT` with the
  verdict header; every body quotes the selection; the pending review is deleted
  when a later step fails; a closed PR is refused. Idempotency: a retry with the
  same `submission_id` after a crash following `submitPullRequestReview` posts no
  second review and sends no second wake; a crash after `addPullRequestReview`
  resumes the pending review; a crash before it starts clean. Re-requesting review
  on an already-reviewed blob shows **Review requested** again; wake message text and
  recipient routing (live author, retired author → parent, none → undelivered).
- Doc token: accepted by the JSON endpoints for its own doc at any SHA, rejected for
  another doc, when expired, or on `/view`.
- CLI: with no `--pr`, the current branch's open PR is used (and `--no-pr` opts out),
  so republishing lands on the same doc.
- Republish authorship: a different session's republish keeps a live author and
  takes over from a retired, killed or unknown one; the author's own republish is
  unchanged.
- Reader default: the readable URL without `?version=` renders the latest published
  SHA, not the PR head, and `/docs/{id}` redirects to the readable URL;
  `/head` reports `pr_head_blob_differs` correctly.
  When Google auth is not enabled, doc routes behave like other routes (no auth
  needed) and the token is ignored.
- Blob SHA: the locally computed value equals the contents API's `sha` for a real file.
- Projection: `docs` and the `owner_review` waiting entry; old-schema Android
  parsing still works.
- Manual: publish a real memo from a worktree, open it on the phone over the
  Cloudflare hostname, switch revisions from the picker (no 401), leave two tap
  comments, submit, confirm the GitHub review and the agent's wake. Repeat on a PR that **modifies** an existing doc, commenting on
  an untouched paragraph, and confirm it lands as a file comment with the quote.

## Non-goals

- Copying or versioning doc bytes in sm.
- Sandboxing agent HTML.
- Replying to or resolving GitHub threads from sm (the agent does that with `gh`; the
  owner can use GitHub).
- Docs outside GitHub-hosted repos.

## Classification

Epic (#1447) with three sub-tickets: #1449, #1450 and #1451.
