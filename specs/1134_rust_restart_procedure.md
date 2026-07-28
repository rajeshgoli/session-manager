# 1134 - Rust server restart procedure

Status: implemented
Related: #1131, #1132 (both took the server down while verifying)

## Problem

Restarting the Rust server after a rebuild took the service down twice on
2026-07-27. The procedure people passed around (`cargo build` then
`launchctl kickstart -k`, sometimes with a `codesign` step) is not safe, and the
one mandatory-sounding step (`codesign`) appeared nowhere in the repo.

## What actually kills the server

Both outages left a crash report in `~/Library/Logs/DiagnosticReports/`:

| time | pid | signing identifier | linker-signed |
| --- | --- | --- | --- |
| 14:24:23 | 41794 | `sm_server-8e8fcd02d48ff250` | yes (`CS_LINKER_SIGNED`) |
| 20:24:35 | 79047 | `sm-server-555549444e2921b67a38394d8635ecbc5a6fa555` | no |

Both report the same termination:

```
termination: { namespace: "CODESIGNING", code: 4, indicator: "Launch Constraint Violation" }
exception:   { type: "EXC_CRASH", signal: "SIGKILL (Code Signature Invalid)" }
```

Three things follow from this, and each contradicts part of the folklore:

1. **It is a launch-constraint violation, not an invalid signature.** The binary
   was validly signed both times. launchd can pin a launch constraint into the
   *job registration*; a rebuilt binary no longer satisfies it and is SIGKILLed
   at spawn. `KeepAlive` turns that single rejection into a crash loop.
2. **`codesign` is not the missing step.** The 14:24 crash happened on a
   binary that was only linker-signed, i.e. with no manual `codesign` at all,
   and it failed identically. `cargo build` already produces a validly ad-hoc
   signed binary on arm64 macOS (`codesign -v` passes on a fresh build).
3. **The signing identifier churns every build.** Ad-hoc signing derives the
   identifier from the Mach-O UUID (`555549444e` is hex for `"UUID"`, followed
   by the UUID itself); the linker derives it from cargo's `deps/` filename.
   Hence the two different identifiers above for the same program.

## Open question from the issue: stable self-signed identity - REJECTED

The issue asked whether signing with a stable self-signed identity instead of
ad-hoc `-s -` would make the per-build cdhash churn irrelevant, removing the
need for bootout/bootstrap. It does not, for three independent reasons.

**It is not the cdhash that breaks.** Nine launchd experiments against a
throwaway label (`com.rajeshgoli.lwcr-probe`, a small C binary, plus one using
the real `sm-server`) showed a manually bootstrapped job survives `kickstart -k`
across arbitrary changes to *both* the cdhash and the signing identifier:

| experiment | result |
| --- | --- |
| linker-signed, rebuild + `codesign -f -s -`, `kickstart -k` | survived, new cdhash |
| rebuild in place while running (clang and codesign both write a new inode) | running process unaffected |
| rebuild with a *different* signing identifier, `kickstart -k` | survived |
| real `sm-server` binary under a test label | no constraint pinned |
| plist in `~/Library/LaunchAgents` rather than a scratch dir | no constraint pinned |
| legacy `launchctl load -w` instead of `bootstrap` | no constraint pinned |

In every manual bootstrap the job reported
`properties = keepalive | runatload | inferred program` - **no** `managed LWCR |
has LWCR`. So stabilising the hash targets a variable that is not the trigger.

**The constraint lives in the registration, not in the binary.** The live job
today runs the *exact binary* whose identifier appears in the 20:24 crash
report, healthily, because recovery re-registered the job with bootout +
bootstrap. Same bytes, same cdhash, same identifier - the only thing that
changed was the registration. No signing scheme can fix a stale registration.

**It is not adoptable here anyway.** `security find-identity -v -p codesigning`
reports `0 valid identities`. Creating a self-signed code-signing certificate
that `codesign` will accept requires adding keychain trust settings, which is an
interactive, machine-local operation. A repo script cannot depend on it, and it
would not reproduce on another machine or a fresh account.

**Not reproduced:** we could not make launchd pin a constraint on demand. The
remaining untested path is launchd's own registration of `~/Library/LaunchAgents`
at login, which we cannot trigger without a reboot and did not attempt with live
sessions on the box. That gap does not change the fix: since a pinned constraint
is cleared only by re-registration, and you cannot tell from outside which kind
of registration is currently live, the restart path must always re-register.

## Fix

`scripts/restart-rust-server.sh` owns the whole sequence. The ordering is the
substance: phase 1 contains everything that can fail without consequence, and
nothing in it touches the service.

```
Phase 1 (service untouched on any failure; nothing writes the registered path)
  record /health and session count (a healthy server must yield a baseline)
  preflight: cutover executable, config readable, local-env readable,
             no lingering Python label, registered path is NOT cargo's output,
             plist would not be rewritten
  cargo build --release -p sm-server --target-dir <pinned>
  cp cargo output -> staging (beside the installed binary)
  codesign --force --sign - --identifier com.rajeshgoli.sm-server <staging>
  codesign --verify --strict <staging>
Phase 2
  cutover stop-rust                     <- bootout
  confirm the job is really unloaded    <- the cutover swallows bootout failures
  install: mv staging -> registered path (atomic, the only write to it)
  cutover start-rust                    <- bootstrap -> kickstart
  poll /health until healthy (timeout -> nonzero)
  require state=running and an unchanged pid for 20s
  require session count not to have dropped
```

### The service must not run out of the build directory

The obvious reading of the acceptance criterion - a failing build or sign leaves
the running service alone - is not enough on its own, and the reason is
structural rather than a missing check.

The service used to be registered against `target/release/sm-server`, which is
also where cargo writes. So a build replaced the live binary while the old
process kept running from its own inode. Any failure after that left a process
alive *now* whose next KeepAlive respawn would use a build the registration had
never accepted - the outage, deferred. Worse, it was not only a failure path: in
the window between cargo writing and the job being re-registered, a server that
merely *exited* would be respawned onto that binary, with the script having done
nothing wrong.

Three separate defensive layers were tried against this (restore on failure,
then make the restore total, then stage the build aside immediately) and each
narrowed the window without closing it. The cause was that cargo's output path
was the registered path, so that is what changed:

- launchd is registered against an installed copy at `.local/bin/sm-server`;
- cargo keeps writing `target/release/sm-server`, with `--target-dir` pinned
  explicitly (it outranks `CARGO_TARGET_DIR` and `build.target-dir`);
- nothing in phase 1 writes the registered path at all, so no rollback is
  needed and there is no window to reason about;
- the registered path is written exactly once, by an atomic rename, *while the
  job is booted out*.

This also removes a hazard that had nothing to do with this script: an ordinary
`cargo build` by anyone working in the repo used to replace the running server's
registered binary. It no longer touches it.

Evidence that the service really was running out of the build directory: the
14:24 crash report's signing identifier is `sm_server-8e8fcd02d48ff250`, which
is exactly what cargo's linker produces for `deps/sm_server-8e8fcd02d48ff250`.
The installed copy now carries the stable `com.rajeshgoli.sm-server` identifier
instead.

Adopting this changes the plist's program path, so the first run reports a plist
divergence and needs `--allow-plist-change` once. That is the guard working as
intended; subsequent runs are silent.

The adoption run itself needs care, and this was easy to miss: before it, the
*live registration* still runs from cargo's output, so simply building during
that run would overwrite the executable the loaded job is using - the very
hazard being migrated away from, on the one run that has not escaped it yet.
The preflight therefore refuses to build whenever the live plist's program is
`SM_CARGO_OUTPUT`, and points at `--adopt`, which installs the build already
running (a read-only copy, no rebuild) and re-registers against the installed
path. After that, normal runs build safely.

### Two restarts at once

Both invocations can pass preflight and both see the job unloaded; one then
installs and starts while the other renames its staged binary over the now-live
registered path. The second `start-rust` fails on the occupied port, leaving the
service running a binary that was replaced outside its registration - the exact
state everything above is arranged to prevent. A per-label lock is taken before
anything is read and held through verification. `mkdir` is the atomic primitive:
macOS ships no `flock(1)`.

A lock whose holder is dead is *reported*, not reclaimed. Testing liveness and
then removing the directory cannot be made atomic in shell - two invocations can
both read the same dead pid, and the second `rm -rf` deletes the lock the first
just created, leaving both believing they own it, which is exactly the
interleaving the lock exists to stop. So nothing removes a lock it does not own;
the error prints the `rm -rf` to run after confirming no restart is in flight.

### Trust the loaded registration, not the plist file

The plist on disk and the registration launchd actually has loaded can disagree -
an edited plist that was never reloaded still leaves launchd executing the old
program. So "is this deployment still running from cargo's output?" is answered
from the loaded job's `program` field when the job is loaded - that is what
launchd will actually exec, so it settles the question - and from the plist only
when the job is not loaded. Consulting the plist as well would refuse a perfectly
safe rebuild whenever an edited plist had not been reloaded, and would send the
operator to `--adopt`, which would redeploy an older artifact. A stale plist is
the divergence guard's business. The related distinct-path invariant is checked in *every*
mode, including `--skip-build` and `--adopt`: a run that does not build must
still not leave the service registered against cargo's output, or the next
ordinary `cargo build` replaces the live binary.

### Preflight the labels the cutover actually enforces

`start_rust` refuses to start alongside a set of Python labels that is hard-coded
in the cutover with no CLI override. So the preflight always checks that set,
regardless of `SM_PYTHON_LABELS` - which is additive, not a replacement. A
narrowed override would otherwise let the preflight pass, the Rust job be
stopped, and `start-rust` then reject the launch, leaving the service down: the
exact outcome the preflight exists to avoid. A test parses both scripts and fails
if the two lists drift apart.

### `--adopt` is only valid before migration

Repeating `--adopt` after the registration already points at the installed binary
would deploy whatever artifact happens to remain in the target directory, which
may be older than what is running - and health, pid, and session-count checks
would all pass over that silent downgrade. Adoption is therefore refused unless
the service really is still registered against cargo's output.

### `bootout` failures are silent

`stop_rust` in the cutover runs `launchctl bootout ... || true` and prints
`stopped` either way, so a job that refused to unload would still look stopped.
Installing then would replace the binary with KeepAlive still able to respawn
it. Phase 2 therefore polls `launchctl print` until the label is genuinely gone
before installing, and aborts with the previous build still in place if it is
not.

### A plist rewrite is a silent config change

`restart-rust` regenerates the plist, so any setting in the live plist that the
script would not regenerate is about to be dropped - a custom `--local-env`
carrying auth secrets being the dangerous case. Rather than enumerate settings,
phase 1 renders the plist the restart *would* write and diffs it against the
live one, aborting on any difference (`--allow-plist-change` to override after
reading the diff). `SM_LOCAL_ENV` is supported and forwarded so the operator can
make the two match. On the current deployment the rendered and live plists are
byte-identical, so the guard is silent in normal use.

Notes on specific choices:

- **Reuses `rust-service-cutover.sh restart-rust`**, which already implements
  bootout -> bootstrap -> kickstart correctly. There is one restart path, not a
  third one.
- **`codesign --verify` is the gate, not `sm-server --help`.** `--help` exits 0
  even when launchd will reject the binary, because a launch constraint is
  enforced by launchd at spawn, not by `exec`.
- **Explicit signing with a stable identifier** is kept even though the linker
  already signs. It costs about a second, ends the per-build identifier churn
  that made the two crash reports look like different programs, and fails fast
  while the service is still safely running.
- **The pid must stay put for 20s.** A single health check cannot distinguish a
  healthy service from one mid-crash-loop, since `KeepAlive` makes each rejected
  spawn briefly look alive.
- **Deployment values are forwarded to the cutover.** `rust-service-cutover.sh`
  initialises its own defaults and reads none of this script's environment
  variables, so `--label`, `--binary`, `--config`, `--host`, and `--port` are
  passed explicitly. Without that, overriding a value would sign and verify one
  deployment while booting out and restarting the default (production) one. For
  the same reason the health URL is built from `SM_HOST`/`SM_PORT` rather than
  being independently settable: the endpoint polled is by construction the one
  the service was told to listen on.
- **A healthy server must yield a session baseline.** If `/health` answers but
  `/sessions` does not, phase 1 aborts rather than continuing with an empty
  baseline - otherwise the post-restart comparison is skipped and a restart that
  dropped the whole registry still reports success. Only a genuinely down server
  (a recovery restart) is allowed to proceed without one.
- **The registered path must NOT be cargo's output.** If they were the same, a
  build would write the live binary directly; the script refuses to run in that
  configuration. (An earlier revision required the opposite - that they match -
  which was the wrong invariant and is superseded.)
- **Session-count drops fail the run** (`--allow-drop N` to tolerate expected
  churn). Sessions do retire on their own - the count moved 13 -> 12 during this
  investigation with no restart - so the escape hatch exists, but the default is
  strict.

## Recovery

If the service is down and the script cannot fix it:

```bash
launchctl bootout   gui/$(id -u)/com.rajeshgoli.session-manager-rust
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.rajeshgoli.session-manager-rust.plist
launchctl kickstart gui/$(id -u)/com.rajeshgoli.session-manager-rust
curl -s localhost:8420/health
```

`kickstart -k` alone will not clear a pinned constraint. Note also that
`bootstrap` on its own did not spawn the job in testing despite `RunAtLoad`
(`state = not running`, `runs = 0`); the explicit `kickstart` is required.

## Coverage

`tests/unit/test_restart_rust_server.py` drives the real script with
cargo/codesign/launchctl/curl replaced by PATH stubs that log every invocation.
The load-bearing tests assert the negative: when the build, the signature, the
verification, or the binary path fails, neither launchd nor the cutover script
is contacted at all.

## Correction to the issue

Acceptance criterion 4 asks that `docs/product/lessons.md` no longer advise
`codesign` + `kickstart -k`. It never did - `codesign` appears nowhere in the
repo, as the issue's own first point says. The lesson entry has instead been
pointed at this script.
