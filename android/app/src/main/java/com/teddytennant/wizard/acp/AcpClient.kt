package com.teddytennant.wizard.acp

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray
import kotlinx.serialization.json.putJsonObject
import java.io.InputStream
import java.io.OutputStream

/** stdin/stdout of a `wizard acp` process, local or over an SSH exec channel. */
interface AcpTransport {
    val input: InputStream
    val output: OutputStream
    fun close()
}

/** What the user picked on a permission prompt; `null` optionId means dismissed. */
data class PermissionAnswer(val optionId: String?)

/**
 * An Agent Client Protocol client for one `wizard acp` process. One process
 * serves any number of sessions.
 */
class AcpClient(
    private val transport: AcpTransport,
    scope: CoroutineScope,
    private val onUpdate: suspend (SessionNotification) -> Unit,
    private val onPermission: suspend (PermissionRequest) -> PermissionAnswer = { PermissionAnswer(null) },
) {
    private val rpc = JsonRpcConnection(
        input = transport.input,
        output = transport.output,
        scope = scope,
        onRequest = ::handleRequest,
        onNotification = ::handleNotification,
    )
    private var readerJob: Job? = null

    val closed get() = rpc.closed

    fun start() {
        if (readerJob == null) readerJob = rpc.start()
    }

    suspend fun initialize(clientVersion: String): AgentInfo {
        start()
        val result = rpc.request("initialize", buildJsonObject {
            put("protocolVersion", 1)
            putJsonObject("clientCapabilities") {
                putJsonObject("fs") {
                    put("readTextFile", false)
                    put("writeTextFile", false)
                }
                put("terminal", false)
            }
            putJsonObject("clientInfo") {
                put("name", "wizard-android")
                put("title", "Wizard for Android")
                put("version", clientVersion)
            }
        })
        return AcpParse.agentInfo(result)
    }

    suspend fun newSession(cwd: String): OpenedSession {
        val result = rpc.request("session/new", buildJsonObject {
            put("cwd", cwd)
            putJsonArray("mcpServers") {}
        })
        return AcpParse.openedSession(result)
    }

    /** Reopens a saved session. Its transcript arrives through [onUpdate], marked as replay, before this returns. */
    suspend fun loadSession(sessionId: String, cwd: String): OpenedSession {
        val result = rpc.request("session/load", buildJsonObject {
            put("sessionId", sessionId)
            put("cwd", cwd)
            putJsonArray("mcpServers") {}
        })
        return AcpParse.openedSession(result, fallbackId = sessionId)
    }

    suspend fun listSessions(cwd: String? = null, cursor: String? = null): SessionPage {
        val result = rpc.request("session/list", buildJsonObject {
            if (cwd != null) put("cwd", cwd)
            if (cursor != null) put("cursor", cursor)
        })
        return AcpParse.sessionPage(result)
    }

    suspend fun setConfigOption(sessionId: String, configId: String, value: String): List<ConfigOption> {
        val result = rpc.request("session/set_config_option", buildJsonObject {
            put("sessionId", sessionId)
            put("configId", configId)
            put("value", value)
        })
        return AcpParse.configOptions((result as? JsonObject)?.get("configOptions"))
    }

    /** Sends a text prompt and suspends until the turn ends. Returns the stop reason. */
    suspend fun prompt(sessionId: String, text: String): String {
        val result = rpc.request("session/prompt", buildJsonObject {
            put("sessionId", sessionId)
            put("prompt", buildJsonArray {
                add(buildJsonObject {
                    put("type", "text")
                    put("text", text)
                })
            })
        })
        return AcpParse.stopReason(result)
    }

    suspend fun cancel(sessionId: String) {
        rpc.notify("session/cancel", buildJsonObject { put("sessionId", sessionId) })
    }

    fun close() {
        rpc.close()
        transport.close()
    }

    private suspend fun handleNotification(method: String, params: JsonElement?) {
        if (method != "session/update") return
        AcpParse.notification(params)?.let { onUpdate(it) }
    }

    private suspend fun handleRequest(method: String, params: JsonElement?): JsonElement = when (method) {
        "session/request_permission" -> {
            val request = AcpParse.permissionRequest(params)
                ?: throw JsonRpcException(-32602, "invalid permission request")
            val answer = onPermission(request)
            buildJsonObject {
                putJsonObject("outcome") {
                    if (answer.optionId == null) {
                        put("outcome", "cancelled")
                    } else {
                        put("outcome", "selected")
                        put("optionId", answer.optionId)
                    }
                }
            }
        }
        // The app advertises no fs or terminal capability, so anything else is unexpected.
        else -> throw JsonRpcException(JsonRpcException.METHOD_NOT_FOUND, "method not found: $method")
    }
}
