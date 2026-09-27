package com.teddytennant.wizard.data

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json

private val Context.dataStore: DataStore<Preferences> by preferencesDataStore(name = "wizard")

enum class ThemeChoice { Dark, Light, System }

data class Settings(
    val theme: ThemeChoice = ThemeChoice.Dark,
    val notifyFinished: Boolean = true,
    val notifyInput: Boolean = true,
    val askedForNotifications: Boolean = false,
    /** Folds each turn's tool calls and thinking into one line, as the desktop's Compact mode. */
    val compact: Boolean = false,
    /** The Starship backdrop behind new Wizard chats, as the desktop's "Wizard artwork". */
    val artwork: Boolean = true,
    val onboarded: Boolean = false,
)

class MachineStore(private val store: DataStore<Preferences>) {
    private val key = stringPreferencesKey("machines")
    private val json = Json { ignoreUnknownKeys = true }
    private val serializer = ListSerializer(Machine.serializer())

    val machines: Flow<List<Machine>> = store.data.map { prefs ->
        prefs[key]?.let { runCatching { json.decodeFromString(serializer, it) }.getOrNull() }.orEmpty()
    }

    suspend fun get(id: String): Machine? = machines.first().firstOrNull { it.id == id }

    suspend fun upsert(machine: Machine) = edit { list ->
        if (list.any { it.id == machine.id }) list.map { if (it.id == machine.id) machine else it } else list + machine
    }

    suspend fun delete(id: String) = edit { list -> list.filterNot { it.id == id } }

    suspend fun update(id: String, change: (Machine) -> Machine) = edit { list -> list.map { if (it.id == id) change(it) else it } }

    private suspend fun edit(change: (List<Machine>) -> List<Machine>) {
        store.edit { prefs ->
            val current = prefs[key]?.let { runCatching { json.decodeFromString(serializer, it) }.getOrNull() }.orEmpty()
            prefs[key] = json.encodeToString(serializer, change(current))
        }
    }
}

class SettingsStore(private val store: DataStore<Preferences>) {
    private val theme = stringPreferencesKey("theme")
    private val finished = booleanPreferencesKey("notify_finished")
    private val input = booleanPreferencesKey("notify_input")
    private val asked = booleanPreferencesKey("asked_notifications")
    private val compact = booleanPreferencesKey("compact")
    private val artwork = booleanPreferencesKey("artwork")
    private val onboarded = booleanPreferencesKey("onboarded")

    val settings: Flow<Settings> = store.data.map { p ->
        Settings(
            theme = p[theme]?.let { runCatching { ThemeChoice.valueOf(it) }.getOrNull() } ?: ThemeChoice.Dark,
            notifyFinished = p[finished] ?: true,
            notifyInput = p[input] ?: true,
            askedForNotifications = p[asked] ?: false,
            compact = p[compact] ?: false,
            artwork = p[artwork] ?: true,
            onboarded = p[onboarded] ?: false,
        )
    }

    suspend fun current(): Settings = settings.first()
    suspend fun setTheme(choice: ThemeChoice) = store.edit { it[theme] = choice.name }
    suspend fun setNotifyFinished(on: Boolean) = store.edit { it[finished] = on }
    suspend fun setNotifyInput(on: Boolean) = store.edit { it[input] = on }
    suspend fun markAskedForNotifications() = store.edit { it[asked] = true }
    suspend fun setCompact(on: Boolean) = store.edit { it[compact] = on }
    suspend fun setArtwork(on: Boolean) = store.edit { it[artwork] = on }
    suspend fun markOnboarded() = store.edit { it[onboarded] = true }

    companion object {
        fun dataStore(context: Context): DataStore<Preferences> = context.dataStore
    }
}
