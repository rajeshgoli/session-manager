# Single-listener proof for #1986

These scratch probes support #1982's inbound-profile correction. They are not a
production listener broker or supported launch adapter. The production contract
is ticket #1986; the sandbox primitive is `scripts/local-wall/`.

`probe-inbound.py` tests the macOS filter limitation: deny all IP inbound, admit
localhost, and attempt a literal loopback address. `probe-inherited-listener.py`
passes a host-created loopback listener into a sandbox and tests accept/reply
versus re-listen. Each driver asserts the expected positive and negative
outcomes; its JSON also records each child exit code.

`probe-listener-shim.c` is a single inherited-listener adapter. The host creates,
binds and listens before passing the descriptor; the adapter substitutes it for
one ordinary loopback bind and avoids a denied re-listen syscall. It does not
implement allocation, authentication, multiple listeners or descriptor reuse.
`probe-listener-shim.py` compiles it, loads it after sandbox-exec's protected
loader has stripped inherited DYLD variables, and checks a Python listener.

`probe-opencode-listener.py` tests the same adapter with unmodified opencode
1.17.9 under the current wall plus an IP-inbound denial. It uses a temporary fake
home/config/password, disables provider updates/model downloads, uses no plugins
or model calls, authenticates a health request, and terminates the fixture.
Result: `{"healthy": true, "version": "1.17.9"}`. The provider's normal HTTP URL
and command arguments remain unchanged.

The checked-in files are evidence/reference material. The original probe paths
are `/tmp/sm-1974/`; scripts refer to those paths and must be staged there to
reproduce. Also copy the current `scripts/local-wall/wall_profile.py` to
`/tmp/sm-1974/local-wall/wall_profile.py`; the opencode probe imports it. Run each
through `sm queue run --type tests` from the ticket worktree.
Do not put the proof adapter in production or relax the sandbox to accommodate
an unsupported executable. macOS SO_ACCEPTCONN is not a supported getsockopt
query here; the production broker must validate and own descriptor allocation.
