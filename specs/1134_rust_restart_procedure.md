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
Phase 1 (service untouched on any failure)
  record /health and session count (a healthy server must yield a baseline)
  preflight: config readable, local-env readable, no lingering Python label,
             SM_BINARY is cargo's output, plist would not be rewritten
  snapshot the registered binary        <- rollback armed
  cargo build --release -p sm-server
  codesign --force --sign - --identifier com.rajeshgoli.sm-server <binary>
  codesign --verify --strict <binary>
Phase 2                                 <- rollback disarmed; the new binary stays
  scripts/rust-service-cutover.sh restart-rust <forwarded deployment args>
  poll /health until healthy (timeout -> nonzero)
  require state=running and an unchanged pid for 20s
  require session count not to have dropped
```

### "Untouched" has to include the binary on disk

The obvious reading of the acceptance criterion - a failing build or sign leaves
the running service alone - is not enough. A build replaces the registered
executable while the old process keeps running from its own inode, so a failure
*after* the build leaves a process that is alive now but whose next KeepAlive
respawn would use a build the live registration never accepted. That is the
outage, just deferred to whenever the service next restarts.

Phase 1 therefore snapshots the binary before the build and restores it if
anything fails before the restart commits. Verified live: with signing forced to
pass and verification forced to fail, the cdhash was restored byte-identical
(`b06ed8fc...` before and after), the pid was unchanged, and no backup file was
left behind.

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
- **`SM_BINARY` must be cargo's output when building.** `cargo build` writes its
  own path, so signing and restarting some other `SM_BINARY` would deploy stale
  code while reporting a fresh build. Mismatch aborts; `--skip-build` is the
  supported way to deploy a prebuilt binary from elsewhere.
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
