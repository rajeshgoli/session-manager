package li.rajeshgo.sm.data.model

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class CreatePlainTerminal(val cwd: String, val name: String? = null)

@Serializable
data class PlainTerminalCreated(val id: String, @SerialName("tmux_session") val tmuxSession: String, val cwd: String)

@Serializable
data class PlainTerminalRouteRequest(val where: String)

@Serializable
data class PlainTerminalRoute(
    val command: String,
    @SerialName("lan_probe_url") val lanProbeUrl: String,
    @SerialName("server_instance") val serverInstance: String,
    @SerialName("ssh_host") val sshHost: String,
    @SerialName("ssh_port") val sshPort: Int,
    @SerialName("ssh_user") val sshUser: String,
    @SerialName("tmux_session") val tmuxSession: String,
)

@Serializable
data class DeviceSshKey(val fingerprint: String, @SerialName("revoked_at") val revokedAt: String? = null)

@Serializable
data class DeviceSshKeys(val keys: List<DeviceSshKey> = emptyList())

@Serializable
data class RegisterDeviceSshKey(@SerialName("public_key") val publicKey: String)

/** Shell rows have no provider transcript and are absent from /client/sessions. */
fun WatchStateSession.toShellSession(): ClientSession = ClientSession(
    id = id, name = name, workingDir = workingDir,
    status = if (state == "stopped") "stopped" else "running",
    createdAt = lastActivity, lastActivity = lastActivity, tmuxSession = id,
    provider = "shell", kind = "shell", activityState = state, facts = facts, attention = attention,
)
