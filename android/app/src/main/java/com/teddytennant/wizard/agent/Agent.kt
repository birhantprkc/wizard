package com.teddytennant.wizard.agent

import androidx.annotation.DrawableRes
import com.teddytennant.wizard.R

/**
 * The coding agents the app can drive, the same set as Wizard GUI's default
 * harnesses. Wizard and Pi speak ACP (`wizard acp`, and the `pi-acp` adapter
 * the desktop app installs); Claude Code speaks its own stream-json.
 */
enum class Agent(val id: String, val displayName: String, @DrawableRes val icon: Int, val brandColor: Long?) {
    Wizard("wizard", "Wizard", R.drawable.ic_wizard_mark, null),
    Pi("pi", "Pi", R.drawable.ic_pi_mark, null),
    // The desktop app keeps Claude's orange even on its monochrome surfaces.
    ClaudeCode("claude", "Claude Code", R.drawable.ic_claude_mark, 0xFFD97757);

    companion object {
        fun fromId(id: String?): Agent = entries.firstOrNull { it.id == id } ?: Wizard
    }
}

/** What a machine has of one agent. */
data class AgentAvailability(val version: String?, val ready: Boolean, val missing: String?) {
    val installed: Boolean get() = version != null

    companion object {
        val Unknown = AgentAvailability(null, false, null)
    }
}
