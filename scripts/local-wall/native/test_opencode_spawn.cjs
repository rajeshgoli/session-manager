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
  const sibling = spawn(process.env.SM_TEST_SHELL, ["-c", "sleep 60"], {
    detached: true, stdio: "ignore",
  })
  for (const signal of ["SIGTERM", "SIGKILL"]) {
    const prefix = signal.toLowerCase()
    const child = spawn(process.env.SM_TEST_SHELL, ["-c",
      `sleep 60 & echo $! > ${prefix}-background.txt; ` +
      `(sleep 60 & echo $! > ${prefix}-grandchild.txt; wait) & ` +
      `echo $! > ${prefix}-child.txt; ` +
      `"$SM_TEST_PYTHON" -c 'import os,time; p=os.fork(); ` +
      `os._exit(0) if p else None; ` +
      `exec("while os.getppid() != 1: time.sleep(0.01)"); ` +
      `exec("try: os.setpgid(0,0)\\nexcept PermissionError: pass\\nelse: raise AssertionError(\\\"helper escaped command group\\\")"); ` +
      `open("${prefix}-orphan.txt","w").write(str(os.getpid())); time.sleep(60)'; wait`,
    ], { detached: true, stdio: "ignore" })
    child.on("error", (error) => { throw error })
    fs.writeFileSync("native-cancel-pid.txt", String(child.pid))
    const files = ["background", "grandchild", "child", "orphan"].map(x => `${prefix}-${x}.txt`)
    const readyBy = Date.now() + 3000
    while (!files.every(file => fs.existsSync(file) && fs.readFileSync(file, "utf8").trim())) {
      assert(Date.now() < readyBy, "command descendants did not start")
      await new Promise(resolve => setTimeout(resolve, 10))
    }
    const pids = files.map(file => Number(fs.readFileSync(file, "utf8").trim()))
    assert.equal(new Set(pids).size, 4)
    await new Promise((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error("cancelled child did not exit")), 3000)
      child.on("exit", (code, actual) => {
        clearTimeout(timeout)
        assert.equal(actual, signal)
        resolve()
      })
      // Match Opencode's kernel group cancellation, including reparented workers.
      process.kill(-child.pid, signal)
    })
    const goneBy = Date.now() + 3000
    const alive = pid => {
      try { process.kill(pid, 0); return true }
      catch (error) { assert.equal(error.code, "ESRCH"); return false }
    }
    while (pids.some(alive)) {
      assert(Date.now() < goneBy, `cancelled descendants survived: ${pids.filter(alive)}`)
      await new Promise(resolve => setTimeout(resolve, 10))
    }
    assert.equal(process.kill(sibling.pid, 0), true, "cancellation affected an unrelated tool")
    await run("git", ["status", "--porcelain"])
  }
  await new Promise(resolve => {
    sibling.on("exit", resolve)
    process.kill(-sibling.pid, "SIGTERM")
  })
  // Re-executing the privileged runtime must not let a helper regroup.
  for (const image of [process.execPath, require("node:path").join(require("node:path").dirname(process.execPath), "launch-env")]) {
    await new Promise((resolve, reject) => {
      try {
        const helper = spawn(image, ["-e", "process.exit(0)"], { stdio: "ignore" })
        helper.on("error", error => error.code === "EPERM" ? resolve() : reject(error))
        helper.on("exit", () => reject(new Error("command helper re-executed privileged image")))
      } catch (error) { error.code === "EPERM" ? resolve() : reject(error) }
    })
  }
  await assert.rejects(run("/usr/bin/env", [process.execPath, "-e", "process.exit(0)"]), /exit (126|127)/)
}

main().catch((error) => { console.error(error); process.exit(1) })
