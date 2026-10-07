// Invoked by the isolated Rust test against the real production judge service.
import assert from "node:assert/strict"
import { SmJudge } from "./sm_judge.js"

const directory = process.env.OPENCODE_TEST_CHECKOUT
const hook = (await SmJudge({ directory }))["tool.execute.before"]
const check = (filePath) => hook({ tool: "edit", sessionID: "ses_test", callID: "path-test" },
  { args: { filePath, oldString: "before", newString: "after" } })

await check("inside.rs")
await check("src/../inside.rs")
await assert.rejects(check("../escape.rs"), /outside/)
await assert.rejects(check(directory + "/../escape.rs"), /outside/)
await assert.rejects(check("escape-link/file.rs"), /outside/)
await assert.rejects(hook({ tool: "apply_patch", sessionID: "ses_test", callID: "move-test" },
  { args: { patchText: "*** Update File: inside.rs\n*** Move to: ../escape.rs" } }), /outside/)
process.env.LOCAL_JUDGE_TOKEN = "wrong-token"
await assert.rejects(check("inside.rs"), /unknown agent/)
console.log("Production judge: relative edit allowed; traversal, absolute escape, symlink escape, patch move and forged credential denied")
