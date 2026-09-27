package com.teddytennant.wizard.session

import com.teddytennant.wizard.acp.PlanEntry
import com.teddytennant.wizard.acp.SessionUpdate
import com.teddytennant.wizard.acp.ToolContent
import com.teddytennant.wizard.acp.ToolStatus

/** One thing on screen in a chat. [key] is stable for the item's lifetime. */
sealed interface TranscriptItem {
    val key: String

    data class User(override val key: String, val text: String) : TranscriptItem
    data class Agent(override val key: String, val text: String) : TranscriptItem
    data class Thinking(override val key: String, val text: String) : TranscriptItem
    data class Tool(
        override val key: String,
        val callId: String,
        val title: String,
        val kind: String?,
        val status: ToolStatus,
        val input: String?,
        val output: List<ToolContent>,
    ) : TranscriptItem
    data class Plan(override val key: String, val entries: List<PlanEntry>) : TranscriptItem
    data class Notice(override val key: String, val text: String, val isError: Boolean = false) : TranscriptItem
}

/**
 * Folds `session/update`s into what the chat shows. Streamed chunks append to
 * the message they continue; a chunk after anything else starts a new one.
 */
object Transcript {
    fun apply(items: List<TranscriptItem>, update: SessionUpdate): List<TranscriptItem> {
        val last = items.lastOrNull()
        return when (update) {
            is SessionUpdate.UserText -> when (last) {
                is TranscriptItem.User -> items.dropLast(1) + last.copy(text = last.text + update.text)
                else -> items + TranscriptItem.User(nextKey(items, "u"), update.text)
            }
            is SessionUpdate.AgentText -> when (last) {
                is TranscriptItem.Agent -> items.dropLast(1) + last.copy(text = last.text + update.text)
                else -> if (update.text.isEmpty()) items else items + TranscriptItem.Agent(nextKey(items, "a"), update.text)
            }
            is SessionUpdate.Thought -> when (last) {
                is TranscriptItem.Thinking -> items.dropLast(1) + last.copy(text = last.text + update.text)
                else -> if (update.text.isEmpty()) items else items + TranscriptItem.Thinking(nextKey(items, "t"), update.text)
            }
            is SessionUpdate.ToolCallStarted -> {
                // Ids only need to be unique within a turn, so an earlier turn's card is never reused.
                val index = indexInTurn(items, update.id)
                val tool = TranscriptItem.Tool(
                    key = if (index >= 0) items[index].key else nextKey(items, "c"),
                    callId = update.id,
                    title = update.title,
                    kind = update.kind,
                    status = update.status,
                    input = update.input,
                    output = update.content,
                )
                if (index >= 0) items.toMutableList().also { it[index] = tool } else items + tool
            }
            is SessionUpdate.ToolCallUpdated -> {
                val index = indexInTurn(items, update.id)
                if (index < 0) return items
                val tool = items[index] as TranscriptItem.Tool
                items.toMutableList().also {
                    it[index] = tool.copy(
                        title = update.title ?: tool.title,
                        status = update.status ?: tool.status,
                        output = update.content ?: tool.output,
                    )
                }
            }
            is SessionUpdate.Plan -> {
                val index = items.indexOfLast { it is TranscriptItem.Plan }
                if (index >= 0 && index == items.lastIndex) {
                    items.dropLast(1) + (items[index] as TranscriptItem.Plan).copy(entries = update.entries)
                } else {
                    items + TranscriptItem.Plan(nextKey(items, "p"), update.entries)
                }
            }
            is SessionUpdate.ConfigOptionsChanged, is SessionUpdate.Other -> items
        }
    }

    private fun indexInTurn(items: List<TranscriptItem>, callId: String): Int {
        val turnStart = items.indexOfLast { it is TranscriptItem.User }
        val index = items.indexOfLast { it is TranscriptItem.Tool && it.callId == callId }
        return if (index > turnStart) index else -1
    }

    fun userMessage(items: List<TranscriptItem>, text: String): List<TranscriptItem> =
        items + TranscriptItem.User(nextKey(items, "u"), text)

    fun notice(items: List<TranscriptItem>, text: String, isError: Boolean = false): List<TranscriptItem> =
        items + TranscriptItem.Notice(nextKey(items, "n"), text, isError)

    /** Tools still marked running when a turn ends did not finish. */
    fun settle(items: List<TranscriptItem>): List<TranscriptItem> = items.map {
        if (it is TranscriptItem.Tool && (it.status == ToolStatus.Running || it.status == ToolStatus.Pending)) {
            it.copy(status = ToolStatus.Failed)
        } else {
            it
        }
    }

    /** The first non-blank line of the last agent message, for a notification. */
    fun lastReplyFirstLine(items: List<TranscriptItem>): String? =
        items.lastOrNull { it is TranscriptItem.Agent }
            ?.let { (it as TranscriptItem.Agent).text }
            ?.lineSequence()
            ?.map { it.trim() }
            ?.filterNot { it.startsWith("```") }
            ?.map { it.trimStart('#', '>', '-', '*', ' ').replace("**", "").replace("`", "") }
            ?.firstOrNull { it.isNotBlank() }

    private fun nextKey(items: List<TranscriptItem>, prefix: String) = "$prefix${items.size}"
}
