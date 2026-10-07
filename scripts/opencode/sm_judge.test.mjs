import { test } from "node:test"
import assert from "node:assert/strict"
import { mkdtempSync, readFileSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { SmJudge } from "./sm_judge.js"

const directory = "/private/tmp/opencode-checkout"
const input = (tool) => ({ tool, sessionID: "ses_test", callID: "call-test" })
const calls = []
let answer = { permissionDecision: "allow", permissionDecisionReason: "rule allow" }
let status = 200
let unavailable = false
globalThis.fetch = async (url, request) => {
  calls.push({ url: String(url), request, body: JSON.parse(request.body) })
  if (unavailable) throw new Error("offline")
  return new Response(JSON.stringify({ hookSpecificOutput: answer }), { status })
}
process.env.LOCAL_AGENT_ID = "agent-test"
process.env.LOCAL_JUDGE_TOKEN = "judge-secret"
process.env.LOCAL_JUDGE_URL = "http://127.0.0.1:8441/decide"
const hook = (await SmJudge({ directory }))["tool.execute.before"]

test("relative paths, bash working folder and all patch endpoints are judged", async () => {
  await hook(input("edit"), { args: { filePath: "src/../main.rs", oldString: "old", newString: "new" } })
  assert.deepEqual(calls.at(-1).body.tool_input,
    { file_path: directory + "/main.rs", old_string: "old", new_string: "new" })
  await hook(input("bash"), { args: { command: "git status", workdir: "src/.." } })
  assert.equal(calls.at(-1).body.cwd, directory)
  const before = calls.length
  await hook(input("apply_patch"), { args: { patchText:
    "*** Begin Patch\n*** Update File: src/a\n*** Move to: ../outside\n*** Add File: b\n*** Delete File: c\n*** End Patch" } })
  assert.deepEqual(calls.slice(before).map(c => c.body.tool_input.file_path),
    [directory + "/src/a", "/private/tmp/outside", directory + "/b", directory + "/c"])
  for (const call of calls) {
    assert.equal(call.request.headers["X-Local-Agent"], "agent-test")
    assert.equal(call.request.headers["X-Local-Judge-Token"], "judge-secret")
    assert.equal(call.body.hook_event_name, "PreToolUse")
    assert.equal(call.request.redirect, "error")
  }
})

test("every tool mapping and web calls reach the judge", async () => {
  for (const [tool, args, name, tin] of [
    ["read", { filePath: "../escape" }, "Read", { file_path: "/private/tmp/escape" }],
    ["write", { filePath: "/outside", content: "body" }, "Write", { file_path: "/outside", content: "body" }],
    ["glob", { path: "src", pattern: "*.rs" }, "Glob", { path: directory + "/src", pattern: "*.rs" }],
    ["list", {}, "Glob", { path: directory, pattern: "" }],
    ["grep", { path: "src", pattern: "find" }, "Grep", { path: directory + "/src", pattern: "find" }],
    ["todowrite", { todos: [] }, "TodoWrite", {}],
    ["webfetch", { url: "https://example.com" }, "webfetch", { url: "https://example.com" }],
    ["websearch", { query: "rust docs" }, "websearch", { query: "rust docs" }],
  ]) {
    await hook(input(tool), { args })
    assert.equal(calls.at(-1).body.tool_name, name)
    assert.deepEqual(calls.at(-1).body.tool_input, tin)
  }
})

test("deny, errors, unreadable answers, missing credentials all fail closed", async () => {
  for (const denied of [{ permissionDecision: "deny", permissionDecisionReason: "outside checkout" },
    {}, { permissionDecision: "ask" }]) {
    answer = denied
    await assert.rejects(hook(input("read"), { args: {} }), /Permission denied by the local judge/)
  }
  answer = { permissionDecision: "allow" }
  status = 503
  await assert.rejects(hook(input("read"), { args: {} }), /judge unavailable/)
  status = 200
  unavailable = true
  await assert.rejects(hook(input("read"), { args: {} }), /judge unavailable/)
  unavailable = false
  delete process.env.LOCAL_JUDGE_TOKEN
  const before = calls.length
  await assert.rejects(hook(input("read"), { args: {} }), /judge unavailable/)
  assert.equal(calls.length, before)
  process.env.LOCAL_JUDGE_TOKEN = "judge-secret"
})

test("patch stops at first denied endpoint and records decisions without secrets", async () => {
  const tmp = mkdtempSync(join(tmpdir(), "opencode-plugin-"))
  process.env.SM_JUDGE_PLUGIN_LOG = join(tmp, "plugin.jsonl")
  try {
    answer = { permissionDecision: "deny", permissionDecisionReason: "outside" }
    const before = calls.length
    await assert.rejects(hook(input("apply_patch"), { args: { patchText:
      "*** Update File: ../outside\n*** Add File: inside" } }), /outside/)
    assert.equal(calls.length, before + 1)
    const log = readFileSync(process.env.SM_JUDGE_PLUGIN_LOG, "utf8")
    assert.equal(JSON.parse(log).allow, false)
    assert.equal(log.includes("judge-secret"), false)
  } finally {
    delete process.env.SM_JUDGE_PLUGIN_LOG
    rmSync(tmp, { recursive: true, force: true })
  }
})
