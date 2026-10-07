#!/usr/bin/env python3
"""Build the static demo site: the real web UI plus a service worker that
replays fixtures/ in place of the server.

    python3 web-demo/build.py                 # writes web-demo/dist/
    python3 web-demo/build.py --serve         # builds, then previews on :8440

The output is a plain static directory; any static host that serves files at
their paths works. See README.md ("The static site").
"""
import argparse
import hashlib
import http.server
import json
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
WEB = os.path.join(REPO, "crates", "sm-server", "src", "web")
SITE = os.path.join(HERE, "site")
FIXTURES = os.path.join(HERE, "fixtures")

# Bare module names the app imports (crates/sm-server/src/http/web.rs, VENDOR_IMPORTS).
VENDOR_IMPORTS = {
    "preact": "vendor/preact.module.js",
    "preact/hooks": "vendor/hooks.module.js",
    "htm": "vendor/htm.module.js",
    "html-to-image": "vendor/html-to-image.js",
}
# The app's page paths (app.js PAGES, plus History's tabs). Each gets a copy
# of the shell so a first visit by deep link works before the worker runs.
PAGE_PATHS = ["board", "queue", "inbox", "notes", "analytics", "history", "history/agents",
              "history/tickets", "guestbook", "settings", "watch"]


def web_files():
    for root, _, names in os.walk(WEB):
        for name in sorted(names):
            path = os.path.join(root, name)
            yield os.path.relpath(path, WEB).replace(os.sep, "/"), path


def build_id():
    digest = hashlib.sha256()
    for rel, path in sorted(web_files()):
        digest.update(rel.encode() + b"\0")
        with open(path, "rb") as f:
            digest.update(f.read() + b"\0")
    with open(os.path.join(FIXTURES, "timeline.json"), "rb") as f:
        digest.update(f.read())
    for name in sorted(os.listdir(SITE)):
        with open(os.path.join(SITE, name), "rb") as f:
            digest.update(f.read())
    return digest.hexdigest()[:12]


def inline_json(value):
    # Safe inside <script>: no `</` sequences (owner_doc_render::inline_json).
    return json.dumps(value, separators=(",", ":")).replace("</", "<\\/")


def shell(build, timeline):
    """The web app's shell document (http/web.rs shell_response), with the
    demo's banner and worker boot in place of the direct module load."""
    imports = {name: f"/assets/{file}?v={build}" for name, file in VENDOR_IMPORTS.items()}
    for rel, _ in web_files():
        if rel.endswith(".js"):
            imports[f"/assets/{rel}"] = f"/assets/{rel}?v={build}"
    config = {
        "build_id": build,
        "server_version": "demo",
        "server_started_at": timeline["ticks"][0]["captured_at"],
        # The recording server's limits (the tests limit was 1 for the story).
        "queue_config_limits": {"max_running": 2, "tests": 1, "perf": 1, "background": 2, "service": 1},
        "refresh_seconds": 3,
        "stall_minutes": 15,
        "owner_name": timeline.get("owner_name", "Alex"),
        "inbox_token": "demo",
    }
    return f"""<!doctype html>
<html lang="en" data-theme="system">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>sm — demo</title>
<meta name="description" content="Session Manager: a recorded sprint by a scripted team, on the real web UI.">
<script>try{{var t=localStorage.getItem("sm-theme");if(t==="light"||t==="dark")document.documentElement.dataset.theme=t}}catch(e){{}}</script>
<script>try{{var s=Number(localStorage.getItem("sm-text-size"));document.documentElement.style.fontSize=(Number.isInteger(s)&&s>=13&&s<=19?s:15)+"px"}}catch(e){{document.documentElement.style.fontSize="15px"}}</script>
<link rel="stylesheet" href="/assets/vendor/xterm.css?v={build}">
<link rel="stylesheet" href="/assets/app.css?v={build}">
<link rel="stylesheet" href="/assets/queue.css?v={build}">
<link rel="stylesheet" href="/demo/demo.css?v={build}">
<script type="importmap">{inline_json({"imports": imports})}</script>
<script type="application/json" id="sm-config">{inline_json(config)}</script>
<script src="/demo/demo.js?v={build}" data-app="/assets/app.js?v={build}" defer></script>
</head>
<body><div id="app"></div></body>
</html>
"""


def build(out):
    with open(os.path.join(FIXTURES, "timeline.json")) as f:
        timeline = json.load(f)
    bid = build_id()
    staging = out + ".partial"
    shutil.rmtree(staging, ignore_errors=True)
    shutil.copytree(WEB, os.path.join(staging, "assets"))
    shutil.copytree(FIXTURES, os.path.join(staging, "fixtures"))
    os.makedirs(os.path.join(staging, "demo"))
    for name in ("demo.js", "demo.css"):
        shutil.copy(os.path.join(SITE, name), os.path.join(staging, "demo", name))
    with open(os.path.join(SITE, "sw.js")) as f:
        worker = f.read()
    with open(os.path.join(staging, "sw.js"), "w") as f:
        f.write(worker.replace("__SM_DEMO_BUILD__", bid))
    page = shell(bid, timeline)
    for rel in ["", *PAGE_PATHS]:
        os.makedirs(os.path.join(staging, rel), exist_ok=True)
        with open(os.path.join(staging, rel, "index.html"), "w") as f:
            f.write(page)
    # Any other deep link (an agent's terminal, a doc opened full screen on a
    # first visit) gets the shell; the worker then answers it.
    with open(os.path.join(staging, "404.html"), "w") as f:
        f.write(page)
    shutil.rmtree(out, ignore_errors=True)
    os.rename(staging, out)
    print(f"built {out} (build {bid})")


class Preview(http.server.SimpleHTTPRequestHandler):
    """Serves like a static host: `/board` is board/index.html (no redirect)
    and an unknown path gets 404.html."""

    def send_head(self):
        path = self.path.split("?", 1)[0].split("#", 1)[0]
        local = self.translate_path(path)
        if os.path.isdir(local) and os.path.isfile(os.path.join(local, "index.html")):
            self.path = path.rstrip("/") + "/index.html"
        elif not os.path.exists(local):
            self.path = "/404.html"
            local = self.translate_path(self.path)
            with open(local, "rb") as f:
                body = f.read()
            self.send_response(404)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            return __import__("io").BytesIO(body)
        return super().send_head()

    def end_headers(self):
        self.send_header("Cache-Control", "no-cache")
        super().end_headers()

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", default=os.path.join(HERE, "dist"))
    parser.add_argument("--serve", action="store_true", help="preview the build on localhost")
    parser.add_argument("--port", type=int, default=8440)
    args = parser.parse_args()
    out = os.path.abspath(args.out)
    build(out)
    if args.serve:
        handler = lambda *a, **k: Preview(*a, directory=out, **k)  # noqa: E731
        server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), handler)
        print(f"preview at http://localhost:{args.port}/ (Ctrl-C stops)")
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
