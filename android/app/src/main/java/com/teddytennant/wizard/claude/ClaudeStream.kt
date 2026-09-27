package com.teddytennant.wizard.claude

import com.teddytennant.wizard.acp.ConfigChoice
import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.acp.SessionUpdate
import com.teddytennant.wizard.acp.ToolContent
import com.teddytennant.wizard.acp.ToolStatus
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonObject

/** What one line of Claude Code's stream-json (or its session log) means to the app. */
sealed interface ClaudeEvent {
    data class Update(val update: SessionUpdate) : ClaudeEvent
    /** The turn ended. [error] is set when it failed. */
    data class Result(val subtype: String, val isError: Boolean, val error: String?, val sessionId: String?) : ClaudeEvent
    /** `can_use_tool`: every request needs an answer or the CLI waits forever. */
    data class ToolPermission(val requestId: String, val toolName: String, val input: JsonObject) : ClaudeEvent
    /**
     * `system/init`: the CLI started a model turn. It sends one for every
     * turn, including the wake turn it runs on its own when a background task
     * finishes.
     */
    data object TurnStarted : ClaudeEvent
    /**
     * Background tasks changed. [all] is the whole set when the CLI lists it
     * (`background_tasks_changed`); otherwise one task [started] or [ended].
     */
    data class Tasks(val all: Set<String>? = null, val started: String? = null, val ended: String? = null) : ClaudeEvent
}

/**
 * Claude Code's stream-json, read the way Wizard GUI's Claude harness reads it
 * (gui/crates/harness/src/claude): text and thinking from the partial-message
 * deltas while live, tool calls from the assistant frames, results from the
 * user frames, the turn's end from `result`. Subagent frames (tagged with a
 * `parent_tool_use_id`) stay out of the main feed.
 *
 * [live] is false for a session log, which has whole messages and no deltas.
 */
class ClaudeNormalizer(private val live: Boolean) {
    private var lastWasText = false
    private var lastWasUser = false

    fun line(line: String): List<ClaudeEvent> {
        val obj = runCatching { json.parseToJsonElement(line) as? JsonObject }.getOrNull() ?: return emptyList()
        return frame(obj)
    }

    fun frame(obj: JsonObject): List<ClaudeEvent> {
        if (obj.str("parent_tool_use_id") != null) return emptyList()
        if ((obj["isSidechain"] as? JsonPrimitive)?.booleanOrNull == true) return emptyList()
        return when (obj.str("type")) {
            "stream_event" -> if (live) streamEvent(obj["event"] as? JsonObject) else emptyList()
            "assistant" -> assistant(obj)
            "user" -> user(obj)
            "result" -> listOf(
                ClaudeEvent.Result(
                    subtype = obj.str("subtype") ?: "success",
                    isError = (obj["is_error"] as? JsonPrimitive)?.booleanOrNull == true || obj.str("subtype")?.startsWith("error") == true,
                    error = obj.str("result")?.takeIf { it.isNotBlank() }
                        ?: (obj["errors"] as? JsonArray)?.joinToString("; ") { (it as? JsonPrimitive)?.contentOrNull ?: it.toString() }?.takeIf { it.isNotBlank() },
                    sessionId = obj.str("session_id"),
                ),
            )
            "system" -> system(obj)
            "control_request" -> {
                val request = obj["request"] as? JsonObject
                if (request?.str("subtype") == "can_use_tool") {
                    listOf(ClaudeEvent.ToolPermission(obj.str("request_id") ?: "", request.str("tool_name") ?: "", request["input"] as? JsonObject ?: JsonObject(emptyMap())))
                } else {
                    emptyList()
                }
            }
            else -> emptyList()
        }
    }

    private fun system(obj: JsonObject): List<ClaudeEvent> = when (obj.str("subtype")) {
        "init" -> {
            // A wake turn's reply is its own message, not more of the last one.
            lastWasText = false
            listOf(ClaudeEvent.TurnStarted)
        }
        "background_tasks_changed" -> listOf(
            ClaudeEvent.Tasks(all = (obj["tasks"] as? JsonArray).orEmpty().mapNotNull { (it as? JsonObject)?.str("task_id") }.toSet()),
        )
        "task_started" -> listOfNotNull(obj.str("task_id")?.let { ClaudeEvent.Tasks(started = it) })
        "task_notification" -> listOfNotNull(obj.str("task_id")?.takeIf { finished(obj.str("status")) }?.let { ClaudeEvent.Tasks(ended = it) })
        "task_updated" -> listOfNotNull(
            obj.str("task_id")?.takeIf { finished((obj["patch"] as? JsonObject)?.str("status")) }?.let { ClaudeEvent.Tasks(ended = it) },
        )
        else -> emptyList()
    }

    private fun finished(status: String?) = status in setOf(
        "completed", "complete", "succeeded", "success", "failed", "errored", "error",
        "killed", "cancelled", "canceled", "stopped", "interrupted",
    )

    private fun streamEvent(event: JsonObject?): List<ClaudeEvent> {
        event ?: return emptyList()
        return when (event.str("type")) {
            "content_block_start" -> {
                val block = event["content_block"] as? JsonObject
                if (block?.str("type") == "text" && lastWasText) listOf(text("\n\n")) else emptyList()
            }
            "content_block_delta" -> {
                val delta = event["delta"] as? JsonObject ?: return emptyList()
                when (delta.str("type")) {
                    "text_delta" -> delta.str("text")?.takeIf { it.isNotEmpty() }?.let { listOf(text(it)) }.orEmpty()
                    "thinking_delta" -> delta.str("thinking")?.takeIf { it.isNotEmpty() }?.let {
                        lastWasText = false
                        listOf(ClaudeEvent.Update(SessionUpdate.Thought(it)))
                    }.orEmpty()
                    else -> emptyList()
                }
            }
            else -> emptyList()
        }
    }

    private fun text(value: String): ClaudeEvent {
        lastWasText = true
        lastWasUser = false
        return ClaudeEvent.Update(SessionUpdate.AgentText(value))
    }

    private fun assistant(obj: JsonObject): List<ClaudeEvent> {
        val blocks = (obj["message"] as? JsonObject)?.get("content") as? JsonArray ?: return emptyList()
        val out = mutableListOf<ClaudeEvent>()
        for (element in blocks) {
            val block = element as? JsonObject ?: continue
            when (block.str("type")) {
                "tool_use" -> {
                    lastWasText = false
                    lastWasUser = false
                    val name = block.str("name") ?: "tool"
                    val input = block["input"] as? JsonObject ?: JsonObject(emptyMap())
                    out += ClaudeEvent.Update(
                        SessionUpdate.ToolCallStarted(
                            id = block.str("id") ?: "",
                            title = toolTitle(name, input),
                            kind = toolKind(name),
                            status = if (live) ToolStatus.Running else ToolStatus.Completed,
                            input = if (input.isEmpty()) null else pretty.encodeToString(JsonElement.serializer(), input),
                            content = emptyList(),
                        ),
                    )
                }
                // Live, text and thinking already arrived as deltas.
                "text" -> if (!live) block.str("text")?.takeIf { it.isNotBlank() }?.let {
                    out += text(if (lastWasText) "\n\n$it" else it)
                }
                "thinking" -> if (!live) block.str("thinking")?.takeIf { it.isNotBlank() }?.let {
                    lastWasText = false
                    out += ClaudeEvent.Update(SessionUpdate.Thought(it))
                }
            }
        }
        return out
    }

    private fun user(obj: JsonObject): List<ClaudeEvent> {
        if ((obj["isMeta"] as? JsonPrimitive)?.booleanOrNull == true) return emptyList()
        val content = (obj["message"] as? JsonObject)?.get("content") ?: return emptyList()
        val out = mutableListOf<ClaudeEvent>()
        fun userText(text: String) {
            // The live turn's prompt is added by the app; a log carries it.
            if (live || !isPrompt(text)) return
            out += ClaudeEvent.Update(SessionUpdate.UserText(if (lastWasUser) "\n\n$text" else text))
            lastWasUser = true
            lastWasText = false
        }
        when (content) {
            is JsonPrimitive -> content.contentOrNull?.let(::userText)
            is JsonArray -> for (element in content) {
                val block = element as? JsonObject ?: continue
                when (block.str("type")) {
                    "text" -> block.str("text")?.let(::userText)
                    "tool_result" -> {
                        val failed = (block["is_error"] as? JsonPrimitive)?.booleanOrNull == true
                        out += ClaudeEvent.Update(
                            SessionUpdate.ToolCallUpdated(
                                id = block.str("tool_use_id") ?: "",
                                title = null,
                                status = if (failed) ToolStatus.Failed else ToolStatus.Completed,
                                content = listOf(ToolContent.Text(resultText(block["content"]))),
                            ),
                        )
                    }
                }
            }
            else -> Unit
        }
        return out
    }

    companion object {
        private val json = Json { ignoreUnknownKeys = true }
        private val pretty = Json { prettyPrint = true }
        private const val MAX_OUTPUT = 6_000

        /** Lines the CLI writes into the log that the user never typed. */
        fun isPrompt(text: String): Boolean {
            val t = text.trimStart()
            return t.isNotBlank() && !t.startsWith("<command-") && !t.startsWith("<local-command") &&
                !t.startsWith("<system-reminder>") && !t.startsWith("<task-notification>") && !t.startsWith("Caveat:") &&
                !t.startsWith("[Request interrupted")
        }

        fun toolKind(name: String): String = when (name) {
            "Bash", "BashOutput", "KillShell" -> "execute"
            "Read", "LS" -> "read"
            "Edit", "MultiEdit", "Write", "NotebookEdit" -> "edit"
            "Grep", "Glob", "WebSearch", "ToolSearch" -> "search"
            "WebFetch" -> "fetch"
            else -> "other"
        }

        /** `Bash: cargo test`, `Read: src/main.rs`, like the desktop's tool rows. */
        fun toolTitle(name: String, input: JsonObject): String {
            val key = when (name) {
                "Bash" -> "command"
                "Read", "Edit", "MultiEdit", "Write" -> "file_path"
                "NotebookEdit" -> "notebook_path"
                "Grep", "Glob" -> "pattern"
                "WebFetch" -> "url"
                "WebSearch" -> "query"
                "Task", "Agent" -> "description"
                "TodoWrite" -> null
                else -> null
            }
            val arg = key?.let { input.str(it) }?.lineSequence()?.firstOrNull()?.trim()
            return if (arg.isNullOrEmpty()) name else "$name: $arg"
        }

        private fun resultText(content: JsonElement?): String {
            val text = when (content) {
                is JsonPrimitive -> content.contentOrNull ?: ""
                is JsonArray -> content.mapNotNull { (it as? JsonObject)?.takeIf { b -> b.str("type") == "text" }?.str("text") }.joinToString("\n")
                else -> ""
            }
            return if (text.length > MAX_OUTPUT) text.take(MAX_OUTPUT) + "\n…" else text
        }

        fun userLine(text: String): String = buildJsonObject {
            put("type", "user")
            putJsonObject("message") {
                put("role", "user")
                put("content", text)
            }
            put("parent_tool_use_id", JsonNull)
        }.toString()

        fun controlRequest(requestId: String, subtype: String): String = buildJsonObject {
            put("type", "control_request")
            put("request_id", requestId)
            putJsonObject("request") { put("subtype", subtype) }
        }.toString()

        /** Allows a tool, passing its (possibly answered) input back. */
        fun allow(requestId: String, input: JsonObject): String = buildJsonObject {
            put("type", "control_response")
            putJsonObject("response") {
                put("subtype", "success")
                put("request_id", requestId)
                putJsonObject("response") {
                    put("behavior", "allow")
                    put("updatedInput", input)
                }
            }
        }.toString()

        /** The models from an `initialize` control response, as a select option like ACP's. */
        fun initializeModels(line: String): List<ConfigChoice>? {
            val obj = runCatching { json.parseToJsonElement(line) as? JsonObject }.getOrNull() ?: return null
            if (obj.str("type") != "control_response") return null
            val body = (obj["response"] as? JsonObject)?.get("response") as? JsonObject ?: return null
            val models = body["models"] as? JsonArray ?: return emptyList()
            return models.mapNotNull { m ->
                val model = m as? JsonObject ?: return@mapNotNull null
                val value = model.str("value") ?: return@mapNotNull null
                ConfigChoice(value, model.str("displayName") ?: value, model.str("description"))
            }
        }

        val EffortChoices = listOf(
            ConfigChoice("default", "Default", "Claude Code's own default"),
            ConfigChoice("low", "Low", null),
            ConfigChoice("medium", "Medium", null),
            ConfigChoice("high", "High", null),
            ConfigChoice("xhigh", "Extra high", null),
            ConfigChoice("max", "Max", null),
        )

        fun options(models: List<ConfigChoice>, model: String, effort: String): List<ConfigOption> = listOf(
            ConfigOption("model", "Model", "Model for this session", "model", model, models.ifEmpty { listOf(ConfigChoice("default", "Default", null)) }),
            ConfigOption("thought_level", "Effort", "Reasoning effort", "thought_level", effort, EffortChoices),
        )

        /** One `S` line of [com.teddytennant.wizard.ssh.RemoteScripts.claudeSessions]. */
        fun sessionLine(line: String): SessionInfo? {
            val parts = line.split('\t', limit = 5)
            if (parts.size < 4 || parts[0] != "S") return null
            val title = parts.getOrNull(4)?.takeIf { it.startsWith("\"") }
                ?.let { runCatching { (json.parseToJsonElement(it) as JsonPrimitive).contentOrNull }.getOrNull() }
                ?.lineSequence()?.firstOrNull { it.isNotBlank() }?.trim()
            // Claude Code writes throwaway logs for its own title generation.
            if (title != null && title.startsWith("You generate session titles")) return null
            val seconds = parts[1].toLongOrNull() ?: 0
            return SessionInfo(
                sessionId = parts[2],
                cwd = parts[3],
                title = title?.let { if (it.length > 100) it.take(99) + "…" else it },
                updatedAt = java.time.Instant.ofEpochSecond(seconds).toString(),
            )
        }
    }
}

private fun JsonObject.str(key: String): String? = (this[key] as? JsonPrimitive)?.takeUnless { it is JsonNull }?.contentOrNull
