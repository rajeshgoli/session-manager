// Runs before every tool and before opencode's own permission check.
// Registration supplies LOCAL_AGENT_ID, LOCAL_JUDGE_URL, LOCAL_JUDGE_TOKEN.
// The log is diagnostic; the authoritative log belongs to the judge service.
import { appendFileSync } from "node:fs"
import { resolve } from "node:path"

const UNAVAILABLE = "judge unavailable; retry this command in a minute"
const abs = (p, directory) => resolve(directory, p || ".")

function claudeShape(tool, args, directory) {
  const a = { ...(args || {}) }
  for (const k of ["filePath", "path"]) if (a[k]) a[k] = abs(a[k], directory)
  switch (tool) {
    case "bash":
      return [["Bash", { command: a.command || "", description: a.description || "" },
        abs(a.workdir, directory)]]
    case "read":
      return [["Read", { file_path: a.filePath || "" }, directory]]
    case "edit":
      return [["Edit", { file_path: a.filePath || "", old_string: a.oldString, new_string: a.newString }, directory]]
    case "write":
      return [["Write", { file_path: a.filePath || "", content: a.content || "" }, directory]]
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
      const text = a.patchText || a.patch || ""
      // A move touches both its Update File source and its Move to destination.
      const paths = [...text.matchAll(/^\*\*\* (?:Add|Update|Delete) File: (.+)$|^\*\*\* Move to: (.+)$/gm)]
        .map((m) => abs((m[1] || m[2]).trim(), directory))
      if (paths.length === 0) return [["ApplyPatch", { patch: text }, directory]]
      return paths.map((p) => ["Edit", { file_path: p }, directory])
    }
    default:
      return [[tool, a, directory]]
  }
}

async function decide(toolName, toolInput, cwd, sessionID) {
  try {
    const agent = process.env.LOCAL_AGENT_ID
    const token = process.env.LOCAL_JUDGE_TOKEN
    const url = new URL(process.env.LOCAL_JUDGE_URL)
    if (!agent || !token || url.protocol !== "http:" || url.hostname !== "127.0.0.1"
      || !url.port || url.username || url.password) return [false, UNAVAILABLE]
    const res = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Local-Agent": agent,
        "X-Local-Judge-Token": token },
      body: JSON.stringify({ hook_event_name: "PreToolUse", tool_name: toolName,
        tool_input: toolInput, cwd, session_id: sessionID }),
      redirect: "error",
      signal: AbortSignal.timeout(45000),
    })
    if (!res.ok) return [false, UNAVAILABLE]
    const out = (await res.json()).hookSpecificOutput || {}
    if (out.permissionDecision === "allow") return [true, out.permissionDecisionReason || ""]
    if (out.permissionDecision === "deny") return [false, out.permissionDecisionReason || UNAVAILABLE]
  } catch {}
  return [false, UNAVAILABLE]
}

export const SmJudge = async ({ directory }) => ({
  "tool.execute.before": async (input, output) => {
    for (const [name, tin, cwd] of claudeShape(input.tool, output.args, directory)) {
      const t0 = Date.now()
      const [ok, reason] = await decide(name, tin, cwd, input.sessionID)
      const log = process.env.SM_JUDGE_PLUGIN_LOG
      if (log) {
        try {
          appendFileSync(log, JSON.stringify({ t: t0 / 1000, ms: Date.now() - t0,
            callID: input.callID, opencode_tool: input.tool, tool: name, allow: ok, reason }) + "\n")
        } catch { throw new Error(`Permission denied by the local judge: ${UNAVAILABLE}`) }
      }
      if (!ok) throw new Error(`Permission denied by the local judge: ${reason}`)
    }
  },
})
