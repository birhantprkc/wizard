package com.teddytennant.wizard.acp

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull

/** A choice in a select config option, e.g. `xai-oauth/grok-4.6`. */
data class ConfigChoice(val value: String, val name: String, val description: String? = null)

/** One of the session's config options (`model`, `thought_level`, `wizard_mode`). */
data class ConfigOption(
    val id: String,
    val name: String,
    val description: String?,
    val category: String?,
    val currentValue: String,
    val choices: List<ConfigChoice>,
) {
    val currentChoice: ConfigChoice? get() = choices.firstOrNull { it.value == currentValue }
}

data class SessionInfo(val sessionId: String, val cwd: String, val title: String?, val updatedAt: String?)

data class SessionPage(val sessions: List<SessionInfo>, val nextCursor: String?)

data class AgentInfo(val name: String, val version: String?, val loadSession: Boolean)

data class OpenedSession(val sessionId: String, val configOptions: List<ConfigOption>)

enum class ToolStatus { Pending, Running, Completed, Failed;
    companion object {
        fun parse(value: String?): ToolStatus? = when (value) {
            "pending" -> Pending
            "in_progress" -> Running
            "completed" -> Completed
            "failed" -> Failed
            else -> null
        }
    }
}

/** What a tool call produced: plain text, or a file diff. */
sealed interface ToolContent {
    data class Text(val text: String) : ToolContent
    data class Diff(val path: String, val oldText: String?, val newText: String) : ToolContent
}

data class PlanEntry(val content: String, val status: String)

/** A `session/update` payload, reduced to what the app shows. */
sealed interface SessionUpdate {
    data class UserText(val text: String) : SessionUpdate
    data class AgentText(val text: String) : SessionUpdate
    data class Thought(val text: String) : SessionUpdate
    data class ToolCallStarted(
        val id: String,
        val title: String,
        val kind: String?,
        val status: ToolStatus,
        val input: String?,
        val content: List<ToolContent>,
    ) : SessionUpdate
    data class ToolCallUpdated(
        val id: String,
        val title: String?,
        val status: ToolStatus?,
        val content: List<ToolContent>?,
    ) : SessionUpdate
    data class Plan(val entries: List<PlanEntry>) : SessionUpdate
    data class ConfigOptionsChanged(val options: List<ConfigOption>) : SessionUpdate
    data class Other(val kind: String) : SessionUpdate
}

data class SessionNotification(val sessionId: String, val update: SessionUpdate, val isReplay: Boolean)

/** An agent asking the user to allow something (`session/request_permission`). */
data class PermissionRequest(
    val sessionId: String,
    val title: String,
    val detail: String?,
    val options: List<PermissionOption>,
)

data class PermissionOption(val optionId: String, val name: String, val kind: String)

object AcpParse {
    private val pretty = Json { prettyPrint = true }

    private fun JsonElement?.obj(): JsonObject? = this as? JsonObject
    private fun JsonObject.string(key: String): String? = (this[key] as? JsonPrimitive)?.contentOrNull
    private fun JsonObject.array(key: String): JsonArray? = this[key] as? JsonArray

    fun agentInfo(result: JsonElement): AgentInfo {
        val obj = result.obj() ?: JsonObject(emptyMap())
        val info = obj["agentInfo"].obj()
        val caps = obj["agentCapabilities"].obj()
        return AgentInfo(
            name = info?.string("title") ?: info?.string("name") ?: "agent",
            version = info?.string("version"),
            loadSession = (caps?.get("loadSession") as? JsonPrimitive)?.booleanOrNull ?: false,
        )
    }

    fun configOptions(element: JsonElement?): List<ConfigOption> =
        (element as? JsonArray).orEmpty().mapNotNull { configOption(it) }

    private fun configOption(element: JsonElement): ConfigOption? {
        val obj = element.obj() ?: return null
        val id = obj.string("id") ?: return null
        val current = obj.string("currentValue") ?: obj.string("value") ?: ""
        return ConfigOption(
            id = id,
            name = obj.string("name") ?: id,
            description = obj.string("description"),
            category = obj.string("category"),
            currentValue = current,
            choices = choices(obj.array("options")),
        )
    }

    /** Choices can be flat or grouped (`{group, name, options: [...]}`); groups are flattened. */
    private fun choices(array: JsonArray?): List<ConfigChoice> = array.orEmpty().flatMap { item ->
        val obj = item.obj() ?: return@flatMap emptyList()
        val nested = obj.array("options")
        if (nested != null) {
            choices(nested)
        } else {
            val value = obj.string("value") ?: return@flatMap emptyList()
            listOf(ConfigChoice(value, obj.string("name") ?: value, obj.string("description")))
        }
    }

    fun openedSession(result: JsonElement, fallbackId: String? = null): OpenedSession {
        val obj = result.obj() ?: JsonObject(emptyMap())
        val id = obj.string("sessionId") ?: fallbackId ?: error("session/new answered without a sessionId")
        return OpenedSession(id, configOptions(obj["configOptions"]))
    }

    fun sessionPage(result: JsonElement): SessionPage {
        val obj = result.obj() ?: JsonObject(emptyMap())
        val sessions = obj.array("sessions").orEmpty().mapNotNull { item ->
            val s = item.obj() ?: return@mapNotNull null
            SessionInfo(
                sessionId = s.string("sessionId") ?: return@mapNotNull null,
                cwd = s.string("cwd") ?: "",
                title = s.string("title"),
                updatedAt = s.string("updatedAt"),
            )
        }
        return SessionPage(sessions, obj.string("nextCursor"))
    }

    fun stopReason(result: JsonElement): String = result.obj()?.string("stopReason") ?: "end_turn"

    fun notification(params: JsonElement?): SessionNotification? {
        val obj = params.obj() ?: return null
        val sessionId = obj.string("sessionId") ?: return null
        val update = obj["update"].obj() ?: return null
        val replay = (obj["_meta"].obj()?.get("isReplay") as? JsonPrimitive)?.booleanOrNull ?: false
        return SessionNotification(sessionId, update(update), replay)
    }

    fun update(obj: JsonObject): SessionUpdate {
        val kind = obj.string("sessionUpdate") ?: return SessionUpdate.Other("")
        return when (kind) {
            "user_message_chunk" -> SessionUpdate.UserText(blockText(obj["content"]))
            "agent_message_chunk" -> SessionUpdate.AgentText(blockText(obj["content"]))
            "agent_thought_chunk" -> SessionUpdate.Thought(blockText(obj["content"]))
            "tool_call" -> SessionUpdate.ToolCallStarted(
                id = obj.string("toolCallId") ?: "",
                title = obj.string("title") ?: "tool",
                kind = obj.string("kind"),
                status = ToolStatus.parse(obj.string("status")) ?: ToolStatus.Pending,
                input = obj["rawInput"]?.let { rawInput(it) },
                content = toolContent(obj["content"]),
            )
            "tool_call_update" -> SessionUpdate.ToolCallUpdated(
                id = obj.string("toolCallId") ?: "",
                title = obj.string("title"),
                status = ToolStatus.parse(obj.string("status")),
                content = obj["content"]?.let { toolContent(it) },
            )
            "plan" -> SessionUpdate.Plan(
                obj.array("entries").orEmpty().mapNotNull { e ->
                    val entry = e.obj() ?: return@mapNotNull null
                    PlanEntry(entry.string("content") ?: "", entry.string("status") ?: "pending")
                },
            )
            "config_option_update", "config_options_update" ->
                SessionUpdate.ConfigOptionsChanged(configOptions(obj["configOptions"]))
            else -> SessionUpdate.Other(kind)
        }
    }

    private fun rawInput(element: JsonElement): String? = when (element) {
        is JsonPrimitive -> element.contentOrNull
        is JsonObject -> if (element.isEmpty()) null else pretty.encodeToString(JsonElement.serializer(), element)
        else -> pretty.encodeToString(JsonElement.serializer(), element)
    }

    /** Text of a content block; non-text blocks (images, resources) become a short marker. */
    fun blockText(element: JsonElement?): String {
        val obj = element.obj() ?: return ""
        return when (obj.string("type")) {
            "text" -> obj.string("text") ?: ""
            "resource_link" -> obj.string("uri") ?: ""
            "image" -> "[image]"
            else -> ""
        }
    }

    fun toolContent(element: JsonElement?): List<ToolContent> = (element as? JsonArray).orEmpty().mapNotNull { item ->
        val obj = item.obj() ?: return@mapNotNull null
        when (obj.string("type")) {
            "content" -> ToolContent.Text(blockText(obj["content"]))
            "diff" -> ToolContent.Diff(
                path = obj.string("path") ?: "",
                oldText = obj.string("oldText"),
                newText = obj.string("newText") ?: "",
            )
            else -> null
        }
    }

    fun permissionRequest(params: JsonElement?): PermissionRequest? {
        val obj = params.obj() ?: return null
        val call = obj["toolCall"].obj()
        return PermissionRequest(
            sessionId = obj.string("sessionId") ?: return null,
            title = call?.string("title") ?: "Wizard wants to continue",
            detail = call?.get("rawInput")?.let { rawInput(it) },
            options = obj.array("options").orEmpty().mapNotNull { o ->
                val opt = o.obj() ?: return@mapNotNull null
                PermissionOption(
                    optionId = opt.string("optionId") ?: return@mapNotNull null,
                    name = opt.string("name") ?: opt.string("optionId")!!,
                    kind = opt.string("kind") ?: "allow_once",
                )
            },
        )
    }
}
