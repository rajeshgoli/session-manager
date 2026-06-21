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
WATCH_SCREEN = (
    Path(__file__).resolve().parents[2]
    / "android-app"
    / "app"
    / "src"
    / "main"
    / "java"
    / "li"
    / "rajeshgo"
    / "sm"
    / "ui"
    / "watch"
    / "WatchScreen.kt"
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


def test_terminal_asset_forwards_alternate_screen_scroll_as_wheel_input():
    source = TERMINAL_ASSET.read_text()

    assert "function terminalUsesAlternateBuffer()" in source
    assert "activeBuffer.type === \"alternate\"" in source
    assert "function terminalMouseTrackingEnabled()" in source
    assert "term.modes.mouseTrackingMode !== \"none\"" in source
    assert "function terminalCellFromClient(clientX, clientY)" in source
    assert "function sendTerminalWheel(lines, clientX, clientY)" in source
    assert "function sendTerminalPageScroll(lines)" in source
    assert "const sequence = lines < 0 ? \"\\x1b[5~\" : \"\\x1b[6~\";" in source
    assert "Math.ceil(Math.abs(lines) / 6)" in source
    assert "function wheelDeltaPixels(event)" in source
    assert "event.deltaMode === WheelEvent.DOM_DELTA_LINE" in source
    assert "event.deltaMode === WheelEvent.DOM_DELTA_PAGE" in source
    assert "return event.deltaY * terminalCellHeight();" in source
    assert "return event.deltaY * terminalViewport().clientHeight;" in source
    assert "const buttonCode = lines < 0 ? 64 : 65;" in source
    assert "const sequence = `\\x1b[<${buttonCode};${cell.col};${cell.row}M`;" in source
    assert "bridgeCall(\"input\", sequence)" in source
    assert "event.stopPropagation()" in source
    assert "scrollTerminalByPixels(wheelDeltaPixels(event), event.clientX, event.clientY)" in source
    assert "return sendTerminalPageScroll(lines);" in source
    assert "return scrollTerminalByLines(lines, clientX, clientY);" in source
    assert "return scrollTerminalByLines(parsed, Number(clientX), Number(clientY));" in source


def test_terminal_webview_converts_compose_drag_coordinates_to_css_pixels():
    source = WATCH_SCREEN.read_text()

    assert "import androidx.compose.ui.platform.LocalDensity" in source
    assert "val density = LocalDensity.current.density" in source
    assert "val claudeDirectPageScroll = terminal.provider == \"claude\"" in source
    assert "onDragStart = { claudePageScrollRemainder = 0f }" in source
    assert "onDragEnd = { claudePageScrollRemainder = 0f }" in source
    assert "onDragCancel = { claudePageScrollRemainder = 0f }" in source
    assert "val cssDelta = -dragAmount / density" in source
    assert "claudePageScrollRemainder += cssDelta" in source
    assert "onPageScroll(true)" in source
    assert "onPageScroll(false)" in source
    assert "val cssX = change.position.x / density" in source
    assert "val cssY = change.position.y / density" in source
    assert "\"window.smScrollPixels($cssDelta, $cssX, $cssY);\"" in source
