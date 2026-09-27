package com.teddytennant.wizard

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.core.splashscreen.SplashScreen.Companion.installSplashScreen
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.teddytennant.wizard.data.Settings
import com.teddytennant.wizard.notify.Notifier
import com.teddytennant.wizard.ui.ChatRoute
import com.teddytennant.wizard.ui.WizardNavHost
import com.teddytennant.wizard.ui.theme.WizardTheme
import com.teddytennant.wizard.ui.theme.isDark

class MainActivity : ComponentActivity() {
    private var pendingChat by mutableStateOf<ChatRoute?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        installSplashScreen()
        super.onCreate(savedInstanceState)
        pendingChat = chatFrom(intent)
        val graph = (application as WizardApp).graph
        setContent {
            val settings by graph.settings.settings.collectAsStateWithLifecycle(initialValue = Settings())
            val dark = isDark(settings.theme)
            LaunchedEffect(dark) {
                val style = if (dark) SystemBarStyle.dark(android.graphics.Color.TRANSPARENT)
                else SystemBarStyle.light(android.graphics.Color.TRANSPARENT, android.graphics.Color.TRANSPARENT)
                enableEdgeToEdge(statusBarStyle = style, navigationBarStyle = style)
            }
            WizardTheme(dark = dark) {
                WizardNavHost(graph, pendingChat, onChatOpened = { pendingChat = null })
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        chatFrom(intent)?.let { pendingChat = it }
    }

    private fun chatFrom(intent: Intent?): ChatRoute? {
        val machine = intent?.getStringExtra(Notifier.EXTRA_MACHINE) ?: return null
        val session = intent.getStringExtra(Notifier.EXTRA_SESSION) ?: return null
        val cwd = intent.getStringExtra(Notifier.EXTRA_CWD) ?: return null
        val agent = intent.getStringExtra(Notifier.EXTRA_AGENT) ?: "wizard"
        return ChatRoute(machine, agent, cwd, session)
    }
}
