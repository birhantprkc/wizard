package com.teddytennant.wizard.ui

import kotlinx.serialization.Serializable

@Serializable data object HomeRoute
@Serializable data object OnboardingRoute
@Serializable data object MachinesRoute
@Serializable data class EditMachineRoute(val machineId: String? = null)
@Serializable data class MachineRoute(val machineId: String)

/** A chat. No [sessionId] starts a new one, sending [prompt] as its first message. */
@Serializable
data class ChatRoute(
    val machineId: String,
    val agent: String,
    val cwd: String,
    val sessionId: String? = null,
    val title: String? = null,
    val prompt: String? = null,
)

@Serializable data object SettingsRoute
@Serializable data object KeysRoute
@Serializable data object AboutRoute
