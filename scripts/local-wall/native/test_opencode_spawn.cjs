// Execute using the pinned Opencode binary's own Bun runtime, inside its wall.
const assert = require("node:assert/strict")
const fs = require("node:fs")
const { spawn } = require("node:child_process")

function run(file, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(file, args, { detached: true, stdio: "ignore" })
    child.on("error", reject)
    child.on("exit", (code) => code === 0 ? resolve() : reject(new Error(`exit ${code}`)))
  })
}

async function main() {
  await run("git", ["status", "--porcelain"])
  await run(process.env.SM_TEST_SHELL, ["-c", "printf native-tool > native-tool.txt"])
  // A tool started through the staged shell must retain the socket adapter.
  await run(process.env.SM_TEST_SHELL, ["-c",
    'exec "$SM_TEST_PYTHON" -c \'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); s.listen(); c=socket.create_connection(s.getsockname()); c.sendall(b"x"); a,_=s.accept(); assert a.recv(1)==b"x"; a.close(); c.close(); s.close()\'',
  ])
  const child = spawn(process.env.SM_TEST_SHELL, ["-c", "sleep 60"], {
    detached: true, stdio: "ignore",
  })
  child.on("error", (error) => { throw error })
  fs.writeFileSync("native-cancel-pid.txt", String(child.pid))
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error("cancelled child did not exit")), 3000)
    child.on("exit", (code, signal) => {
      clearTimeout(timeout)
      assert.equal(signal, "SIGTERM")
      resolve()
    })
    // Match Opencode's group-first cancellation and direct-child fallback.
    setTimeout(() => {
      assert.throws(() => process.kill(-child.pid, "SIGTERM"), { code: "ESRCH" })
      assert.equal(child.kill("SIGTERM"), true)
    }, 100)
  })
}

main().catch((error) => { console.error(error); process.exitCode = 1 })
