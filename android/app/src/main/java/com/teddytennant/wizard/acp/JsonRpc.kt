package com.teddytennant.wizard.acp

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import java.io.BufferedReader
import java.io.IOException
import java.io.InputStream
import java.io.InputStreamReader
import java.io.OutputStream
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

class JsonRpcException(val code: Int, message: String, val data: JsonElement? = null) : Exception(message) {
    companion object {
        const val PARSE_ERROR = -32700
        const val METHOD_NOT_FOUND = -32601
        const val INTERNAL_ERROR = -32603
    }
}

/** The connection went away before an answer came back. */
class ConnectionClosedException(message: String, cause: Throwable? = null) : IOException(message, cause)

/** One decoded line of JSON-RPC 2.0. */
sealed interface RpcMessage {
    data class Request(val id: JsonElement, val method: String, val params: JsonElement?) : RpcMessage
    data class Notification(val method: String, val params: JsonElement?) : RpcMessage
    data class Response(val id: Long, val result: JsonElement?, val error: JsonRpcException?) : RpcMessage
    data class Invalid(val line: String, val reason: String) : RpcMessage

    companion object {
        val json = Json { ignoreUnknownKeys = true; explicitNulls = false }

        fun parse(line: String): RpcMessage {
            val obj = try {
                json.parseToJsonElement(line) as? JsonObject
            } catch (e: Exception) {
                return Invalid(line, e.message ?: "not JSON")
            } ?: return Invalid(line, "not an object")
            val id = obj["id"]?.takeUnless { it is JsonNull }
            val method = (obj["method"] as? JsonPrimitive)?.contentOrNull
            return when {
                method != null && id != null -> Request(id, method, obj["params"])
                method != null -> Notification(method, obj["params"])
                id != null -> {
                    val numeric = (id as? JsonPrimitive)?.longOrNull
                        ?: return Invalid(line, "response id is not one of ours")
                    val error = (obj["error"] as? JsonObject)?.let { err ->
                        JsonRpcException(
                            code = (err["code"] as? JsonPrimitive)?.intOrNull ?: JsonRpcException.INTERNAL_ERROR,
                            message = (err["message"] as? JsonPrimitive)?.contentOrNull ?: "error",
                            data = err["data"],
                        )
                    }
                    Response(numeric, obj["result"], error)
                }
                else -> Invalid(line, "neither a request nor a response")
            }
        }

        fun encodeRequest(id: Long, method: String, params: JsonElement?): String = buildJsonObject {
            put("jsonrpc", "2.0")
            put("id", id)
            put("method", method)
            if (params != null) put("params", params)
        }.toString()

        fun encodeNotification(method: String, params: JsonElement?): String = buildJsonObject {
            put("jsonrpc", "2.0")
            put("method", method)
            if (params != null) put("params", params)
        }.toString()

        fun encodeResult(id: JsonElement, result: JsonElement): String = buildJsonObject {
            put("jsonrpc", "2.0")
            put("id", id)
            put("result", result)
        }.toString()

        fun encodeError(id: JsonElement, error: JsonRpcException): String = buildJsonObject {
            put("jsonrpc", "2.0")
            put("id", id)
            put("error", buildJsonObject {
                put("code", error.code)
                put("message", error.message ?: "error")
                error.data?.let { put("data", it) }
            })
        }.toString()
    }
}

/**
 * Newline-delimited JSON-RPC 2.0 over a pair of streams, the framing
 * `wizard acp` speaks on stdin/stdout.
 *
 * Notifications are handed over in the order they arrive, on the reader.
 * Requests from the other side run in their own coroutine, because answering
 * one (a permission prompt) can wait on the user.
 */
class JsonRpcConnection(
    input: InputStream,
    private val output: OutputStream,
    private val scope: CoroutineScope,
    private val onRequest: suspend (method: String, params: JsonElement?) -> JsonElement,
    private val onNotification: suspend (method: String, params: JsonElement?) -> Unit,
) {
    private val reader = BufferedReader(InputStreamReader(input, Charsets.UTF_8))
    private val pending = ConcurrentHashMap<Long, CompletableDeferred<JsonElement>>()
    private val nextId = AtomicLong(0)
    private val writeLock = Mutex()

    @Volatile
    private var closedBy: Throwable? = null
    val closed = CompletableDeferred<Throwable?>()

    fun start(): Job = scope.launch(Dispatchers.IO) {
        var failure: Throwable? = null
        try {
            while (true) {
                val line = reader.readLine() ?: break
                if (line.isBlank()) continue
                dispatch(RpcMessage.parse(line))
            }
        } catch (e: IOException) {
            failure = e
        } finally {
            shutdown(failure)
        }
    }

    private suspend fun dispatch(message: RpcMessage) {
        when (message) {
            is RpcMessage.Response -> {
                val waiter = pending.remove(message.id) ?: return
                if (message.error != null) waiter.completeExceptionally(message.error)
                else waiter.complete(message.result ?: JsonNull)
            }
            is RpcMessage.Notification -> runCatching { onNotification(message.method, message.params) }
            is RpcMessage.Request -> scope.launch {
                val reply = try {
                    RpcMessage.encodeResult(message.id, onRequest(message.method, message.params))
                } catch (e: JsonRpcException) {
                    RpcMessage.encodeError(message.id, e)
                } catch (e: Exception) {
                    RpcMessage.encodeError(message.id, JsonRpcException(JsonRpcException.INTERNAL_ERROR, e.message ?: "error"))
                }
                runCatching { write(reply) }
            }
            is RpcMessage.Invalid -> Unit
        }
    }

    suspend fun request(method: String, params: JsonElement?): JsonElement {
        closedBy?.let { throw ConnectionClosedException("connection closed", it) }
        val id = nextId.getAndIncrement()
        val answer = CompletableDeferred<JsonElement>()
        pending[id] = answer
        try {
            write(RpcMessage.encodeRequest(id, method, params))
        } catch (e: IOException) {
            pending.remove(id)
            throw ConnectionClosedException("couldn't send $method", e)
        }
        if (closed.isCompleted) answer.completeExceptionally(ConnectionClosedException("connection closed"))
        return answer.await()
    }

    suspend fun notify(method: String, params: JsonElement?) {
        write(RpcMessage.encodeNotification(method, params))
    }

    private suspend fun write(line: String) = writeLock.withLock {
        withContext(Dispatchers.IO) {
            output.write((line + "\n").toByteArray(Charsets.UTF_8))
            output.flush()
        }
    }

    private fun shutdown(cause: Throwable?) {
        closedBy = cause ?: ConnectionClosedException("the other side closed the connection")
        val error = ConnectionClosedException("connection closed", cause)
        pending.values.forEach { it.completeExceptionally(error) }
        pending.clear()
        closed.complete(cause)
    }

    fun close() {
        runCatching { output.close() }
        runCatching { reader.close() }
    }
}

internal val JsonElement.objOrNull: JsonObject? get() = this as? JsonObject
internal fun JsonElement?.str(key: String): String? = (this?.let { runCatching { it.jsonObject }.getOrNull() }?.get(key) as? JsonPrimitive)
    ?.takeUnless { it is JsonNull }?.contentOrNull
