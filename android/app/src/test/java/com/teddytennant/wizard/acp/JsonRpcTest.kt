package com.teddytennant.wizard.acp

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.plus
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.BufferedReader
import java.io.InputStreamReader
import java.io.PipedInputStream
import java.io.PipedOutputStream

class JsonRpcTest {
    @Test
    fun parsesEachMessageShape() {
        val response = RpcMessage.parse("""{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}""")
        assertTrue(response is RpcMessage.Response)
        assertEquals(3L, (response as RpcMessage.Response).id)

        val error = RpcMessage.parse("""{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"Method not found"}}""")
        assertEquals(-32601, (error as RpcMessage.Response).error!!.code)

        val note = RpcMessage.parse("""{"jsonrpc":"2.0","method":"session/update","params":{}}""")
        assertEquals("session/update", (note as RpcMessage.Notification).method)

        val request = RpcMessage.parse("""{"jsonrpc":"2.0","id":"abc","method":"session/request_permission","params":{}}""")
        assertEquals(JsonPrimitive("abc"), (request as RpcMessage.Request).id)

        assertTrue(RpcMessage.parse("not json") is RpcMessage.Invalid)
        assertTrue(RpcMessage.parse("[1,2]") is RpcMessage.Invalid)
        assertTrue(RpcMessage.parse("""{"jsonrpc":"2.0","id":"x","result":{}}""") is RpcMessage.Invalid)
    }

    @Test
    fun encodedMessagesAreSingleLines() {
        val line = RpcMessage.encodeRequest(7, "session/prompt", buildJsonObject { put("text", "a\nb") })
        assertTrue('\n' !in line)
        assertEquals("""{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"text":"a\nb"}}""", line)
    }

    /** A fake agent on the other end of two pipes. */
    private class Peer {
        val toClient = PipedOutputStream()
        val clientIn = PipedInputStream(toClient, 1 shl 16)
        val clientOut = PipedOutputStream()
        private val fromClient = BufferedReader(InputStreamReader(PipedInputStream(clientOut, 1 shl 16)))

        fun readLine(): String = fromClient.readLine()
        fun send(line: String) {
            toClient.write((line + "\n").toByteArray())
            toClient.flush()
        }
    }

    @Test
    fun matchesResponsesToRequestsOutOfOrderAndKeepsNotificationsInOrder() = runBlocking {
        val peer = Peer()
        val seen = mutableListOf<String>()
        val rpc = JsonRpcConnection(
            input = peer.clientIn,
            output = peer.clientOut,
            scope = this + Dispatchers.IO,
            onRequest = { _, _ -> JsonObject(emptyMap()) },
            onNotification = { _, params -> seen += (params as JsonObject)["n"]!!.jsonPrimitive.content },
        )
        val reader = rpc.start()
        val first = async(Dispatchers.IO) { rpc.request("a", null) }
        val second = async(Dispatchers.IO) { rpc.request("b", null) }
        val sent = listOf(peer.readLine(), peer.readLine()).map { RpcMessage.parse(it) as RpcMessage.Request }
        val idOf = sent.associate { it.method to it.id.jsonPrimitive.content }
        peer.send("""{"jsonrpc":"2.0","method":"note","params":{"n":"1"}}""")
        peer.send("""{"jsonrpc":"2.0","method":"note","params":{"n":"2"}}""")
        peer.send("""{"jsonrpc":"2.0","id":${idOf["b"]},"result":"B"}""")
        peer.send("""{"jsonrpc":"2.0","id":${idOf["a"]},"result":"A"}""")
        withTimeout(5_000) {
            assertEquals("A", first.await().jsonPrimitive.content)
            assertEquals("B", second.await().jsonPrimitive.content)
        }
        assertEquals(listOf("1", "2"), seen)
        peer.toClient.close()
        reader.join()
    }

    @Test
    fun answersIncomingRequestsAndRejectsUnknownOnes() = runBlocking {
        val peer = Peer()
        val rpc = JsonRpcConnection(
            input = peer.clientIn,
            output = peer.clientOut,
            scope = this + Dispatchers.IO,
            onRequest = { method, _ ->
                if (method == "ping") JsonPrimitive("pong")
                else throw JsonRpcException(JsonRpcException.METHOD_NOT_FOUND, "nope")
            },
            onNotification = { _, _ -> },
        )
        val reader = rpc.start()
        peer.send("""{"jsonrpc":"2.0","id":"p1","method":"ping"}""")
        val ok = peer.readLine()
        peer.send("""{"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{}}""")
        val err = peer.readLine()
        assertEquals("""{"jsonrpc":"2.0","id":"p1","result":"pong"}""", ok)
        assertTrue(err.contains("\"code\":-32601"))
        assertTrue(err.contains("\"id\":9"))
        peer.toClient.close()
        reader.join()
    }

    @Test
    fun pendingRequestsFailWhenTheStreamCloses() = runBlocking {
        val peer = Peer()
        val rpc = JsonRpcConnection(peer.clientIn, peer.clientOut, this + Dispatchers.IO, { _, _ -> JsonObject(emptyMap()) }, { _, _ -> })
        val reader = rpc.start()
        val waiting = async(Dispatchers.IO) { runCatching { rpc.request("session/prompt", null) } }
        peer.readLine()
        peer.toClient.close()
        val outcome = withTimeout(5_000) { waiting.await() }
        assertTrue(outcome.exceptionOrNull() is ConnectionClosedException)
        reader.join()
        try {
            rpc.request("again", null)
            fail("a closed connection took a request")
        } catch (_: ConnectionClosedException) {
        }
    }

    @Test
    fun errorResponsesSurfaceAsExceptions() = runBlocking {
        val peer = Peer()
        val rpc = JsonRpcConnection(peer.clientIn, peer.clientOut, this + Dispatchers.IO, { _, _ -> JsonObject(emptyMap()) }, { _, _ -> })
        val reader = rpc.start()
        val call = async(Dispatchers.IO) { runCatching { rpc.request("session/set_mode", null) } }
        val id = (RpcMessage.parse(peer.readLine()) as RpcMessage.Request).id
        peer.send("""{"jsonrpc":"2.0","id":$id,"error":{"code":-32601,"message":"Method not found"}}""")
        val error = withTimeout(5_000) { call.await() }.exceptionOrNull() as JsonRpcException
        assertEquals(-32601, error.code)
        assertEquals("Method not found", error.message)
        peer.toClient.close()
        reader.join()
    }
}
