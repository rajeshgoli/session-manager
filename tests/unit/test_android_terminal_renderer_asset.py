from pathlib import Path


TERMINAL_ASSET = (
    Path(__file__).resolve().parents[2]
    / "android-app"
    / "app"
    / "src"
    / "main"
    / "assets"
    / "sm_terminal"
    / "terminal.html"
)


def test_terminal_asset_waits_for_layout_before_reporting_ready():
    source = TERMINAL_ASSET.read_text()

    assert "function hasLayout()" in source
    assert "getBoundingClientRect()" in source
    assert "function viewportDimensions()" in source
    assert "terminalElement.style.width" in source
    assert "window.setTimeout" in source
    assert "window.requestAnimationFrame(() =>" in source
    assert "bridgeCall(\"ready\", term.cols, term.rows)" in source


def test_terminal_asset_queues_output_until_renderer_is_ready():
    source = TERMINAL_ASSET.read_text()

    assert "let pendingWrites = [];" in source
    assert "pendingWrites.push(frame)" in source
    assert "flushPendingWrites()" in source
    assert "window.smWriteBase64 = function(sequence, payload)" in source
    assert "window.smWriteText = function(sequence, text)" in source


def test_terminal_asset_reports_write_acks_and_renderer_errors():
    source = TERMINAL_ASSET.read_text()

    assert "bridgeCall(\"written\", String(frame.sequence), byteCount)" in source
    assert "bridgeCall(\"error\", message)" in source
    assert "Terminal write failed" in source
