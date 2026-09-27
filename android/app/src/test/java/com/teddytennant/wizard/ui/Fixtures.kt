package com.teddytennant.wizard.ui

import com.teddytennant.wizard.acp.ConfigChoice
import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.acp.SessionUpdate
import com.teddytennant.wizard.acp.ToolContent
import com.teddytennant.wizard.acp.ToolStatus
import com.teddytennant.wizard.data.AuthKind
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.data.RecentChat
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.agent.AgentAvailability
import com.teddytennant.wizard.session.AgentSession
import com.teddytennant.wizard.ui.screens.HomeState
import com.teddytennant.wizard.ui.screens.RecentRow
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
    val buildbox = Machine("m1", "buildbox", "buildbox", 22, "dev", AuthKind.Key, "k1", listOf("/home/dev/src/wizard", "/home/dev/src/tracer"))
    val cluster = Machine("m2", "gpu node", "gpu-node", 22, "ops", AuthKind.Key, "k1")
    val pi = Machine("m3", "garage pi", "garage-pi", 2222, "pi", AuthKind.Password)

    val allAgents = mapOf(
        Agent.Wizard to AgentAvailability("wizard 3.5", true, null),
        Agent.Pi to AgentAvailability("0.87.1", true, null),
        Agent.ClaudeCode to AgentAvailability("2.1.283 (Claude Code)", true, null),
    )
    val buildboxStatus = MachineStatus(Reach.Online, allAgents, running = 2, home = "/home/dev")

    val cards = listOf(
        MachineCardModel(buildbox, buildboxStatus),
        MachineCardModel(cluster, MachineStatus(Reach.Online, mapOf(Agent.Pi to AgentAvailability("0.87.1", false, "Needs the pi-acp adapter")), home = "/home/ops")),
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
        AgentSession(Agent.Wizard, SessionInfo("2026-09-26T21-02-11", "/home/dev/src/wizard", "Make the ACP session list page past 50 entries", ago(14))),
        AgentSession(Agent.ClaudeCode, SessionInfo("0f38ff65-2fa0-426a-bc3e-03203e166ceb", "/home/dev/src/tracer", "Why does seed 3 diverge after step 1200?", ago(260))),
        AgentSession(Agent.Pi, SessionInfo("01a0e0fa-2226-736a-a086-3e3b60fab1ad", "/home/dev/src/wizard", "Bump sshj and rerun the tests", ago(60 * 26))),
        AgentSession(Agent.Wizard, SessionInfo("2026-09-22T22-31-40", "/home/dev/src/site", "Draft the essays index page", ago(60 * 24 * 4))),
    )

    private fun minutesAgo(m: Long) = Instant.now().minus(m, ChronoUnit.MINUTES).toEpochMilli()

    val recents = listOf(
        RecentRow(RecentChat("m1", "wizard", "2026-09-26T21-02-11", "/home/dev/src/wizard", "Make the ACP session list page past 50 entries", minutesAgo(3)), "buildbox", running = true),
        RecentRow(RecentChat("m2", "claude", "0f38ff65", "/home/ops/train", "Why does seed 3 diverge after step 1200?", minutesAgo(52)), "gpu node", running = false),
        RecentRow(RecentChat("m1", "pi", "01a0e0fa", "/home/dev/src/tracer", "Bump sshj and rerun the tests", minutesAgo(60 * 5)), "buildbox", running = false),
        RecentRow(RecentChat("m3", "wizard", "2026-09-24T08-10-00", "/home/pi/sprinkler", "Water the lawn at dawn, skip rainy days", minutesAgo(60 * 30)), "garage pi", running = false),
        RecentRow(RecentChat("m1", "claude", "e286ecdc", "/home/dev/src/site", "Draft the essays index page", minutesAgo(60 * 24 * 4)), "buildbox", running = false),
    )

    fun home(agent: Agent = Agent.Wizard, artwork: Boolean = true) = HomeState(
        machine = buildbox,
        status = buildboxStatus,
        agent = agent,
        cwd = "/home/dev/src/wizard",
        recents = recents,
        refreshing = false,
        hasMachines = true,
        artwork = artwork,
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
        agent = Agent.Wizard,
        sessionId = "2026-09-26T21-02-11",
        cwd = "/home/dev/src/wizard",
        title = "Make the ACP session list page past 50 entries",
        items = streamed,
        running = true,
        options = options,
    )

    val claudeChat = ChatState(
        machineId = "m2",
        agent = Agent.ClaudeCode,
        sessionId = "0f38ff65-2fa0-426a-bc3e-03203e166ceb",
        cwd = "/home/ops/train",
        title = "Why does seed 3 diverge after step 1200?",
        items = listOf(
            SessionUpdate.Thought("Compare the loss curves before touching the optimizer."),
            SessionUpdate.ToolCallStarted("t1", "Bash: python plot_seeds.py --seeds 0 3 --from 1000", "execute", ToolStatus.Completed, null, listOf(ToolContent.Text("seed 0: 2.114 -> 2.098\nseed 3: 2.117 -> 9.842 (step 1203)"))),
            SessionUpdate.ToolCallStarted("t2", "Read: train/schedule.py", "read", ToolStatus.Completed, null, emptyList()),
            SessionUpdate.AgentText("Seed 3 blows up at step 1203, right where warmup ends. The schedule jumps the learning rate from `3e-4` to `1e-3` in one step instead of ramping.\n\nWant me to make the jump a 200-step ramp and rerun seed 3?"),
        ).fold(Transcript.userMessage(emptyList(), "Why does seed 3 diverge after step 1200?")) { acc, u -> Transcript.apply(acc, u) },
        options = listOf(
            ConfigOption("model", "Model", null, "model", "default", listOf(ConfigChoice("default", "Default (recommended)", null), ConfigChoice("sonnet", "Sonnet 5", null))),
            ConfigOption("thought_level", "Effort", null, "thought_level", "high", listOf(ConfigChoice("default", "Default", null), ConfigChoice("high", "High", null))),
        ),
    )

    val claudeQuestion = claudeChat.copy(
        running = true,
        permission = com.teddytennant.wizard.session.PendingPermission(
            com.teddytennant.wizard.acp.PermissionRequest(
                "0f38ff65", "Which split should the rerun evaluate on?", null,
                listOf("validation", "test", "both").map { com.teddytennant.wizard.acp.PermissionOption(it, it, "allow_once") },
            ),
        ),
    )

    val piChat = ChatState(
        machineId = "m1",
        agent = Agent.Pi,
        sessionId = "01a0e0fa-2226-736a-a086-3e3b60fab1ad",
        cwd = "/home/dev/src/tracer",
        title = "Bump sshj and rerun the tests",
        items = listOf(
            SessionUpdate.ToolCallStarted("p1", "edit gradle/libs.versions.toml", "edit", ToolStatus.Completed, null, listOf(ToolContent.Diff("gradle/libs.versions.toml", "sshj = \"0.40.0\"", "sshj = \"0.41.1\""))),
            SessionUpdate.ToolCallStarted("p2", "bash ./gradlew test", "execute", ToolStatus.Completed, null, listOf(ToolContent.Text("BUILD SUCCESSFUL in 41s\n128 tests completed"))),
            SessionUpdate.AgentText("Bumped sshj to **0.41.1**. All 128 tests pass."),
        ).fold(Transcript.userMessage(emptyList(), "Bump sshj and rerun the tests")) { acc, u -> Transcript.apply(acc, u) },
        options = listOf(
            ConfigOption("model", "Model", null, "model", "xai/grok-4.6", listOf(ConfigChoice("xai/grok-4.6", "xai/Grok 4.6", null))),
            ConfigOption("thought_level", "Thinking", null, "thought_level", "medium", listOf(ConfigChoice("medium", "Thinking: medium", null))),
        ),
    )

    val presented = PresentedKey(
        "buildbox", 22, "ssh-ed25519",
        Base64.getDecoder().decode("AAAAC3NzaC1lZDI1NTE5AAAAIIsfxMidb6GmIlWavPk0xG+uL39oOwR5Kca60Y5PWBR7"),
    )
}
