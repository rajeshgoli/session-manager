// Use the cursor mode negotiated by the running terminal application.
function terminalKeySequence(key, applicationCursorKeys) {
  const arrows = { up: "A", down: "B", right: "C", left: "D" };
  if (Object.prototype.hasOwnProperty.call(arrows, key)) {
    return "\x1b" + (applicationCursorKeys ? "O" : "[") + arrows[key];
  }
  const keys = { enter: "\r", esc: "\x1b", tab: "\t", "shift-tab": "\x1b[Z", backspace: "\x7f", "ctrl-c": "\x03" };
  return Object.prototype.hasOwnProperty.call(keys, key) ? keys[key] : "";
}
