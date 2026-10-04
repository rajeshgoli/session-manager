// #1966: opencode judge plugin. The opencode counterpart of #1954's judge_hook.sh (memo K.3).
//
// opencode runs `tool.execute.before` for every tool call, before the tool and before opencode's
// own permission check. This plugin translates the call into the Claude Code PreToolUse JSON the
// #1954 judge service already understands, posts it, and throws on anything but an explicit allow.
// A throw blocks the call; the model sees the message as the tool's error. opencode's own
// permission table is all "allow" or "deny" (never "ask"), so no call can wait on a human.
//
// Env: LOCAL_AGENT_ID (judge service agent key), LOCAL_JUDGE_PORT (default 8431).
import { appendFileSync } from "fs"

const PORT = process.env.LOCAL_JUDGE_PORT || "8431"
const AGENT = process.env.LOCAL_AGENT_ID || "unknown"
const LOG = process.env.SM_JUDGE_PLUGIN_LOG
const UNAVAILABLE = "judge unavailable; retry this command in a minute"

// opencode may pass paths relative to the session directory; the judge service resolves paths
// against its own working directory, so send absolute paths.
const abs = (p, directory) => (!p || p.startsWith("/") ? p || "" : `${directory}/${p}`)

// opencode tool -> [Claude Code tool name, Claude Code tool_input]
function claudeShape(tool, args, directory) {
  const a = { ...(args || {}) }
  for (const k of ["filePath", "path"]) if (a[k]) a[k] = abs(a[k], directory)
  switch (tool) {
    case "bash":
      return [["Bash", { command: a.command || "", description: a.description || "" }, a.workdir || directory]]
    case "read":
      return [["Read", { file_path: a.filePath || "" }, directory]]
    case "edit":
      return [["Edit", { file_path: a.filePath || "", old_string: a.oldString, new_string: a.newString }, directory]]
    case "write":
      return [["Write", { file_path: a.filePath || "", content: "" }, directory]]
    case "glob":
    case "list":
      return [["Glob", { pattern: a.pattern || "", path: a.path || directory }, directory]]
    case "grep":
      return [["Grep", { pattern: a.pattern || "", path: a.path || directory }, directory]]
    case "todowrite":
    case "todoread":
      return [["TodoWrite", {}, directory]]
    case "apply_patch":
    case "patch": {
      // One Edit check per file the patch touches, so a path outside the checkout is denied.
      const text = a.patchText || a.patch || ""
      const paths = [...text.matchAll(/^\*\*\* (?:Add|Update|Delete) File: (.+)$|^\*\*\* Move to: (.+)$/gm)]
        .map((m) => (m[1] || m[2]).trim())
        .map((p) => (p.startsWith("/") ? p : `${directory}/${p}`))
      if (paths.length === 0) return [["ApplyPatch", { patch: text.slice(0, 2000) }, directory]]
      return paths.map((p) => ["Edit", { file_path: p }, directory])
    }
    default:
      // No rule: the judge service sends it to the judge, which denies anything doubtful.
      return [[tool, a, directory]]
  }
}

async function decide(toolName, toolInput, cwd, sessionID) {
  const body = JSON.stringify({
    hook_event_name: "PreToolUse",
    tool_name: toolName,
    tool_input: toolInput,
    cwd,
    session_id: sessionID,
  })
  try {
    const res = await fetch(`http://127.0.0.1:${PORT}/decide`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Local-Agent": AGENT },
      body,
      signal: AbortSignal.timeout(45000),
    })
    const out = (await res.json()).hookSpecificOutput || {}
    if (out.permissionDecision === "allow") return [true, out.permissionDecisionReason || ""]
    if (out.permissionDecision === "deny") return [false, out.permissionDecisionReason || UNAVAILABLE]
  } catch (e) {}
  return [false, UNAVAILABLE]
}

export const SmJudge = async ({ directory }) => ({
  "tool.execute.before": async (input, output) => {
    for (const [name, tin, cwd] of claudeShape(input.tool, output.args, directory)) {
      const t0 = Date.now()
      const [ok, reason] = await decide(name, tin, cwd, input.sessionID)
      if (LOG) {
        appendFileSync(LOG, JSON.stringify({ t: t0 / 1000, ms: Date.now() - t0, callID: input.callID,
          opencode_tool: input.tool, tool: name, allow: ok, reason }) + "\n")
      }
      if (!ok) throw new Error(`Permission denied by the local judge: ${reason}`)
    }
  },
})
