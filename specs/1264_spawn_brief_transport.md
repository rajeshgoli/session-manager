# 1264: Atomic spawn-brief transport

`sm spawn` accepts exactly one initial-prompt source:

```bash
sm spawn claude "short positional prompt"
sm spawn codex --prompt-file /path/to/brief.md
generate-brief | sm spawn claude --prompt-stdin
```

The file and stdin forms read UTF-8 bytes without caller-shell interpolation.
Before Session Manager allocates a session or starts a runtime, it writes the
accepted brief to `spawn-briefs/<sha256>.md` beside `sessions.json`, with private
directory/file permissions. Publication syncs the file and directory before it
records a launch-intent entry containing the
source metadata, artifact digest, requested provider/model/effort/name, parent,
node, and working directory.

The server verifies and reads the persisted artifact back after acceptance, then
delivers those accepted bytes to the runtime through stdin; the runtime never
launches from either a caller-controlled path or an artifact path. Runtime launch
records retain the intent ID and digest. Unsupported remote runtime nodes continue
to fail explicitly before a child is launched.

Do not use a stand-by spawn followed by `sm send` for a large implementation
brief. Use the atomic file or stdin transport instead.
