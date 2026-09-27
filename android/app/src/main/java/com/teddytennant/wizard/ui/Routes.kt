package com.teddytennant.wizard.ui

import kotlinx.serialization.Serializable

@Serializable data object MachinesRoute
@Serializable data class EditMachineRoute(val machineId: String? = null)
@Serializable data class MachineRoute(val machineId: String)
@Serializable data class ChatRoute(val machineId: String, val cwd: String, val sessionId: String? = null, val title: String? = null)
@Serializable data object SettingsRoute
@Serializable data object KeysRoute
@Serializable data object AboutRoute
