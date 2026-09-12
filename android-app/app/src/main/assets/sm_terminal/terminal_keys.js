// Use the cursor mode negotiated by the running terminal application.
function terminalKeySequence(key, applicationCursorKeys) {
  const arrows = { up: "A", down: "B", right: "C", left: "D" };
  if (Object.prototype.hasOwnProperty.call(arrows, key)) {
    return "\x1b" + (applicationCursorKeys ? "O" : "[") + arrows[key];
  }
  return "";
}
