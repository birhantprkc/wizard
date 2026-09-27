package com.teddytennant.wizard.data

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import com.teddytennant.wizard.agent.Agent
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json

/** A chat on any machine with any agent, for the one list on the home screen. */
@Serializable
data class RecentChat(
    val machineId: String,
    val agentId: String,
    val sessionId: String,
    val cwd: String,
    val title: String? = null,
    /** Epoch millis of the last activity. */
    val updatedAt: Long,
) {
    val agent: Agent get() = Agent.fromId(agentId)
    val key: String get() = "$machineId/$agentId/$sessionId"
}

/** Merging rules, apart from storage so they're tested on the JVM. */
object RecentChats {
    const val LIMIT = 200

    /** Newer activity wins; a known title is never replaced by none. */
    fun merge(existing: List<RecentChat>, incoming: List<RecentChat>): List<RecentChat> {
        val byKey = LinkedHashMap<String, RecentChat>()
        existing.forEach { byKey[it.key] = it }
        incoming.forEach { chat ->
            val old = byKey[chat.key]
            byKey[chat.key] = if (old == null) chat else chat.copy(
                title = chat.title ?: old.title,
                updatedAt = maxOf(old.updatedAt, chat.updatedAt),
                cwd = chat.cwd.ifEmpty { old.cwd },
            )
        }
        return byKey.values.sortedByDescending { it.updatedAt }.take(LIMIT)
    }

    fun forgetMachine(existing: List<RecentChat>, machineId: String) = existing.filterNot { it.machineId == machineId }

}

class RecentStore(private val store: DataStore<Preferences>) {
    private val key = stringPreferencesKey("recents")
    private val json = Json { ignoreUnknownKeys = true }
    private val serializer = ListSerializer(RecentChat.serializer())

    val recents: Flow<List<RecentChat>> = store.data.map { decode(it[key]) }

    suspend fun current(): List<RecentChat> = recents.first()

    suspend fun upsert(chats: List<RecentChat>) = edit { RecentChats.merge(it, chats) }

    suspend fun removeMachine(machineId: String) = edit { RecentChats.forgetMachine(it, machineId) }

    private fun decode(raw: String?): List<RecentChat> =
        raw?.let { runCatching { json.decodeFromString(serializer, it) }.getOrNull() }.orEmpty()

    private suspend fun edit(change: (List<RecentChat>) -> List<RecentChat>) {
        store.edit { prefs -> prefs[key] = json.encodeToString(serializer, change(decode(prefs[key]))) }
    }
}

/** What the new-chat composer had picked last time. */
data class Selection(val machineId: String? = null, val agent: Agent = Agent.Wizard, val cwdByMachine: Map<String, String> = emptyMap()) {
    fun cwd(machineId: String?): String? = machineId?.let { cwdByMachine[it] }
}

class SelectionStore(private val store: DataStore<Preferences>) {
    private val machine = stringPreferencesKey("last_machine")
    private val agent = stringPreferencesKey("last_agent")
    private val dirs = stringPreferencesKey("last_dirs")
    private val json = Json

    val selection: Flow<Selection> = store.data.map { p ->
        Selection(
            machineId = p[machine],
            agent = Agent.fromId(p[agent]),
            cwdByMachine = p[dirs]?.let { runCatching { json.decodeFromString<Map<String, String>>(it) }.getOrNull() }.orEmpty(),
        )
    }

    suspend fun current(): Selection = selection.first()
    suspend fun setMachine(id: String) = store.edit { it[machine] = id }
    suspend fun setAgent(value: Agent) = store.edit { it[agent] = value.id }
    suspend fun setCwd(machineId: String, cwd: String) = store.edit { p ->
        val current = p[dirs]?.let { runCatching { json.decodeFromString<Map<String, String>>(it) }.getOrNull() }.orEmpty()
        p[dirs] = json.encodeToString(current + (machineId to cwd))
    }
}
