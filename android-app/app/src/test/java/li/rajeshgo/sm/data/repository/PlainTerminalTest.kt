package li.rajeshgo.sm.data.repository

import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.*
import li.rajeshgo.sm.data.remote.ApiService
import li.rajeshgo.sm.ui.watch.rowFactsLine
import li.rajeshgo.sm.ui.watch.sessionDisplayName
import li.rajeshgo.sm.util.termuxPublicKey
import li.rajeshgo.sm.util.termuxShellSetupScript
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.*
import org.junit.Test
import java.nio.file.Files

class PlainTerminalTest {
    private val json = Json { ignoreUnknownKeys = true }
    private val home = PlainTerminalRoute("ssh studio-lan", "https://studio-lan.rajeshgo.li:8443/client/terminal/probe", "server-1", "studio-lan.rajeshgo.li", 22222, "rajesh", "term-abcd")

    @Test fun shellRowsKeepTheirFactsAndNeverBecomeAgents() {
        val shell = json.decodeFromString<WatchStateResponse>("""{"sessions":[{"id":"term-abcd","kind":"shell","name":"zsh · project","state":"working","working_dir":"/tmp/project","last_activity":"2026-10-18T12:00:00Z","facts":{"text":"running · cargo","agent":{"state":"working"},"jobs":{"running":0,"waiting":0,"text":null},"you":null,"finished":null},"attention":{"section":"moving","tone":"neutral"}}]}""").sessions.single().toShellSession()
        assertEquals("shell", shell.kind)
        assertEquals("▹ zsh · project", sessionDisplayName(shell))
        assertEquals("running · cargo", rowFactsLine(shell))
        assertEquals("moving", shell.attention?.section)
        assertNull(shell.mobileTerminal)
        assertNull(shell.facts?.you)
        assertNull(shell.facts?.jobs?.text)
    }

    @Test fun createRouteKeysAndRetireUseShellEndpoints() = runBlocking {
        val seen = mutableListOf<String>()
        val client = okhttp3.OkHttpClient.Builder().addInterceptor { chain ->
            val request = chain.request()
            val body = okio.Buffer().also { request.body?.writeTo(it) }.readUtf8()
            seen += "${request.method} ${request.url.encodedPath} $body"
            val response = when {
                request.url.encodedPath.endsWith("/route") -> json.encodeToString(PlainTerminalRoute.serializer(), home)
                request.method == "POST" && request.url.encodedPath == "/terminals" -> """{"id":"term-abcd","cwd":"/tmp","tmux_session":"term-abcd"}"""
                request.method == "GET" -> """{"keys":[{"fingerprint":"SHA256:old","revoked_at":"yesterday"},{"fingerprint":"SHA256:live","revoked_at":null}]}"""
                else -> "{}"
            }
            okhttp3.Response.Builder().request(request).protocol(okhttp3.Protocol.HTTP_1_1).code(200).message("OK")
                .body(response.toResponseBody("application/json".toMediaType())).build()
        }.build()
        val api = retrofit2.Retrofit.Builder().baseUrl("https://example.com/").client(client)
            .addConverterFactory(json.asConverterFactory("application/json".toMediaType())).build().create(ApiService::class.java)
        assertEquals("term-abcd", api.createPlainTerminal(CreatePlainTerminal("/tmp", "Build")).id)
        assertEquals(1, api.deviceSshKeys("phone").keys.count { it.revokedAt == null })
        api.registerDeviceSshKey("phone", RegisterDeviceSshKey("ssh-ed25519 AAAA"))
        assertEquals(home, api.plainTerminalRoute("term-abcd", PlainTerminalRouteRequest("home")))
        api.plainTerminalRoute("term-abcd", PlainTerminalRouteRequest("away"))
        assertTrue(api.retirePlainTerminal("term-abcd").isSuccessful)
        assertEquals(listOf(
            "POST /terminals {\"cwd\":\"/tmp\",\"name\":\"Build\"}", "GET /devices/phone/ssh-keys ",
            "POST /devices/phone/ssh-keys {\"public_key\":\"ssh-ed25519 AAAA\"}",
            "POST /terminals/term-abcd/route {\"where\":\"home\"}",
            "POST /terminals/term-abcd/route {\"where\":\"away\"}", "DELETE /terminals/term-abcd "), seen)
    }

    @Test fun setupPreservesPrivateKeyAndOtherHostsAcrossRetries() {
        val dir = Files.createTempDirectory("sm-termux-test").toFile()
        try {
            val ssh = dir.resolve(".ssh").apply { mkdirs() }
            ssh.resolve("config").writeText("Host other\n  HostName other.example\n")
            val bin = dir.resolve("bin").apply { mkdirs() }
            bin.resolve("cloudflared").apply { writeText("#!/bin/sh\nexit 0\n"); setExecutable(true) }
            // Use bash -c so the temporary HOME is not replaced by a host login profile.
            fun setup(): String {
                val process = ProcessBuilder("/bin/bash", "-c", termuxShellSetupScript(home)).apply {
                    environment()["HOME"] = dir.path
                    environment()["PATH"] = "${bin.path}:/usr/bin:/bin"
                }.start()
                val stdout = process.inputStream.bufferedReader().readText()
                val stderr = process.errorStream.bufferedReader().readText()
                assertEquals(stderr, 0, process.waitFor())
                return termuxPublicKey(stdout)
            }
            val key = setup()
            val privateKey = ssh.resolve("id_ed25519").readText()
            ssh.resolve("id_ed25519.pub").delete()
            assertEquals(key, setup())
            assertEquals(privateKey, ssh.resolve("id_ed25519").readText())
            val config = ssh.resolve("config").readText()
            assertEquals(1, Regex("Host studio-away").findAll(config).count())
            assertTrue(config.contains("Host other\n  HostName other.example"))
            assertTrue(config.contains("cloudflared access ssh --hostname %h"))
            assertFalse(key.contains("PRIVATE"))
        } finally { dir.deleteRecursively() }
    }

    @Test fun setupRejectsUnsafeMetadataAndUnexpectedOutput() {
        assertThrows(IllegalArgumentException::class.java) { termuxShellSetupScript(home.copy(sshUser = "a\nHost *")) }
        assertThrows(IllegalArgumentException::class.java) { termuxPublicKey("ssh-rsa AAAA") }
        assertThrows(IllegalArgumentException::class.java) { termuxPublicKey("ssh-ed25519 AAAA\nextra") }
    }

    @Test fun lanProbeRequiresMatchingInstanceAndBoundsAnUnresponsiveServer() = runBlocking {
        suspend fun probe(instance: String?, silent: Boolean = false): Boolean {
            val release = java.util.concurrent.CountDownLatch(1)
            java.net.ServerSocket(0).use { server ->
                val worker = kotlin.concurrent.thread(isDaemon = true) {
                    server.accept().use { socket ->
                        val reader = socket.getInputStream().bufferedReader()
                        while (!reader.readLine().isNullOrEmpty()) { /* consume request */ }
                        if (silent) release.await() else {
                            val body = "{\"instance\":\"$instance\"}"
                            socket.getOutputStream().write("HTTP/1.1 200 OK\r\nContent-Length: ${body.length}\r\n\r\n$body".toByteArray())
                        }
                    }
                }
                try {
                    return SessionManagerRepository().plainTerminalAtHome(home.copy(lanProbeUrl = "http://127.0.0.1:${server.localPort}/client/terminal/probe"))
                } finally { release.countDown(); worker.join(2000) }
            }
        }
        assertTrue(probe("server-1"))
        assertFalse(probe("another-server"))
        val start = System.nanoTime()
        assertFalse(probe(null, silent = true))
        assertTrue("LAN deadline must not wait for a silent server", (System.nanoTime() - start) / 1_000_000 < 1800)
    }
}
