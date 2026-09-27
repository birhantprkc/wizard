package com.teddytennant.wizard.ui

import com.teddytennant.wizard.acp.ConfigChoice
import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.acp.SessionUpdate
import com.teddytennant.wizard.acp.ToolContent
import com.teddytennant.wizard.acp.ToolStatus
import com.teddytennant.wizard.data.AuthKind
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.session.ChatState
import com.teddytennant.wizard.session.MachineStatus
import com.teddytennant.wizard.session.Reach
import com.teddytennant.wizard.session.Transcript
import com.teddytennant.wizard.session.TranscriptItem
import com.teddytennant.wizard.ssh.PresentedKey
import com.teddytennant.wizard.ssh.StoredKey
import com.teddytennant.wizard.ui.screens.MachineCardModel
import java.time.Instant
import java.time.temporal.ChronoUnit
import java.util.Base64

/** Screenshot data, shaped like what `wizard acp` 3.5 actually sends. */
object Fixtures {
    val devbox = Machine("m1", "devbox", "devbox.local", 22, "teddy", AuthKind.Key, "k1", listOf("/home/teddy/code/wizard", "/home/teddy/code/reverie"))
    val cluster = Machine("m2", "ncshare login", "login.ncshare.org", 22, "tt187", AuthKind.Key, "k1")
    val pi = Machine("m3", "garage pi", "192.168.1.40", 2222, "pi", AuthKind.Password)

    val cards = listOf(
        MachineCardModel(devbox, MachineStatus(Reach.Online, "wizard 3.5", running = 2, home = "/home/teddy")),
        MachineCardModel(cluster, MachineStatus(Reach.Online, null, home = "/hpc/home/tt187")),
        MachineCardModel(pi, MachineStatus(Reach.Offline, message = "Nothing answered. Check the host, port and network.")),
    )

    val key = StoredKey(
        "k1",
        "wizard@pixel-9",
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIsfxMidb6GmIlWavPk0xG+uL39oOwR5Kca60Y5PWBR7 wizard@pixel-9",
        imported = false,
        createdAt = 0,
    )
    val laptopKey = StoredKey(
        "k2",
        "laptop",
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKypliER+iYdXdMLiJVrTTNSHiET7UICjRe4fpxvo69D laptop",
        imported = true,
        createdAt = 0,
    )

    private fun ago(minutes: Long) = Instant.now().minus(minutes, ChronoUnit.MINUTES).toString()

    val sessions = listOf(
        SessionInfo("2026-09-26T21-02-11", "/home/teddy/code/wizard", "Make the ACP session list page past 50 entries", ago(14)),
        SessionInfo("2026-09-26T17-40-03", "/home/teddy/code/reverie", "Why does seed 3 diverge after step 1200?", ago(260)),
        SessionInfo("2026-09-25T09-12-55", "/home/teddy/code/wizard", "Bump sshj and rerun the gauntlet", ago(60 * 26)),
        SessionInfo("2026-09-22T22-31-40", "/home/teddy/code/personal-website", "Draft the essays index page", ago(60 * 24 * 4)),
    )

    val options = listOf(
        ConfigOption(
            "model", "Model", "Provider and model this session runs", "model", "xai-oauth/grok-4.6",
            listOf(
                ConfigChoice("xai-oauth/grok-4.7", "grok-4.7 (xai-oauth)", "xAI · xai-oauth"),
                ConfigChoice("xai-oauth/grok-4.6", "grok-4.6 (xai-oauth)", "xAI · xai-oauth · configured"),
                ConfigChoice("xai-oauth/grok-4.5", "grok-4.5 (xai-oauth)", "xAI · xai-oauth"),
                ConfigChoice("chatgpt/gpt-5.6-sol", "gpt-5.6-sol (chatgpt)", "OpenAI · chatgpt"),
                ConfigChoice("openrouter/anthropic/claude-sonnet-5", "anthropic/claude-sonnet-5 (openrouter)", "OpenRouter · openrouter"),
            ),
        ),
        ConfigOption(
            "thought_level", "Reasoning", "Reasoning effort", "thought_level", "high",
            listOf(
                ConfigChoice("default", "Default", "The provider's own default"),
                ConfigChoice("low", "Low", "Fastest, least reasoning"),
                ConfigChoice("medium", "Medium", "Balanced"),
                ConfigChoice("high", "High", "Deeper reasoning"),
                ConfigChoice("xhigh", "Extra high", "Grok 4.6 and later; others treat it as high"),
            ),
        ),
        ConfigOption(
            "wizard_mode", "Mode", "Wizard's personality mode", null, "genie",
            listOf(
                ConfigChoice("genie", "Genie", "Interactive: acts on each request and reports back"),
                ConfigChoice("sovereign", "Sovereign", "Autonomous: keeps working toward the goal on its own"),
            ),
        ),
    )

    val streamed: List<TranscriptItem> = listOf(
        SessionUpdate.Thought("The list is capped by the first page. session/list returns nextCursor, so the client just stops early."),
        SessionUpdate.AgentText("The cap comes from the client, not the server. `session/list` already pages; we only read the first page."),
        SessionUpdate.ToolCallStarted("c1", "read_file: src/plugins/acp.rs", "read", ToolStatus.Completed, "{\n  \"path\": \"src/plugins/acp.rs\",\n  \"offset\": 520\n}", listOf(ToolContent.Text("pub async fn list_sessions(&self, args: ListSessionsRequest)\n    let page = summaries.chunks(PAGE).nth(cursor).unwrap_or_default();"))),
        SessionUpdate.ToolCallStarted("c2", "execute: cargo test -p wizard acp::list", "execute", ToolStatus.Completed, null, listOf(ToolContent.Text("test acp::list::pages_past_fifty ... ok\ntest result: ok. 1 passed"))),
        SessionUpdate.AgentText("Fixed. The loop now follows `nextCursor`:\n\n```kotlin\nwhile (cursor != null) {\n    val page = client.listSessions(cwd, cursor)\n    all += page.sessions\n    cursor = page.nextCursor\n}\n```\n\n- **3 pages** load on open, then more on scroll\n- the test covers 120 sessions"),
        SessionUpdate.ToolCallStarted("c3", "execute: git commit -am \"Page session/list past the first 50\"", "execute", ToolStatus.Running, "{\n  \"command\": \"git commit -am ...\"\n}", emptyList()),
    ).fold(Transcript.userMessage(emptyList(), "The machine screen only shows 50 sessions. Make it page through all of them.")) { acc, u -> Transcript.apply(acc, u) }

    val chat = ChatState(
        machineId = "m1",
        sessionId = "2026-09-26T21-02-11",
        cwd = "/home/teddy/code/wizard",
        title = "Make the ACP session list page past 50 entries",
        items = streamed,
        running = true,
        options = options,
    )

    val presented = PresentedKey(
        "devbox.local", 22, "ssh-ed25519",
        Base64.getDecoder().decode("AAAAC3NzaC1lZDI1NTE5AAAAIIsfxMidb6GmIlWavPk0xG+uL39oOwR5Kca60Y5PWBR7"),
    )
}
