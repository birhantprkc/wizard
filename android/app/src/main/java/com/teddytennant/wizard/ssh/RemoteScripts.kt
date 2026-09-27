package com.teddytennant.wizard.ssh

import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.agent.AgentAvailability

/**
 * Shell run on the remote. Every command goes through `sh -c '<script>'` so a
 * login shell of fish or zsh on the other end doesn't change the meaning.
 */
object RemoteScripts {
    const val INSTALL_URL = "https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh"

    /** The adapter Wizard GUI pins for Pi, and where it installs it. Sharing the path means one install serves both apps. */
    const val PI_ACP_VERSION = "0.0.33"
    private const val PI_ACP_DIR = "\$HOME/.zeron/adapters/pi-acp/$PI_ACP_VERSION"

    /**
     * Where installers put agents, which a non-interactive SSH shell often
     * lacks, then the login shell's PATH for node version managers. Wizard
     * GUI searches the same places.
     */
    private val PATH_SETUP = """
        export PATH="${'$'}HOME/.local/bin:${'$'}HOME/.cargo/bin:${'$'}HOME/.claude/local:${'$'}HOME/.npm-global/bin:/usr/local/bin:/opt/homebrew/bin:${'$'}PATH"
        if [ -n "${'$'}{SHELL:-}" ] && [ -x "${'$'}SHELL" ]; then
          if command -v timeout >/dev/null 2>&1; then lp=${'$'}(timeout 5 "${'$'}SHELL" -lc env 2>/dev/null </dev/null | sed -n 's/^PATH=//p' | tail -n 1)
          else lp=${'$'}("${'$'}SHELL" -lc env 2>/dev/null </dev/null | sed -n 's/^PATH=//p' | tail -n 1); fi
          [ -n "${'$'}lp" ] && PATH="${'$'}PATH:${'$'}lp"
        fi
    """.trimIndent()

    /** Prints the pi-acp entry point: one on PATH, else the managed install. */
    private val FIND_PI_ACP = """
        piacp=${'$'}(command -v pi-acp 2>/dev/null || true)
        if [ -z "${'$'}piacp" ] && [ -f "$PI_ACP_DIR/.zeron-install-ok" ] && [ -e "$PI_ACP_DIR/node_modules/.bin/pi-acp" ]; then
          piacp="$PI_ACP_DIR/node_modules/.bin/pi-acp"
        fi
    """.trimIndent()

    /** POSIX single quoting; `'\''` also reads correctly in fish. */
    fun quote(value: String): String = "'" + value.replace("'", "'\\''") + "'"

    fun sh(script: String, vararg args: String): String =
        "sh -c " + quote(script) + " sh" + args.joinToString("") { " " + quote(it) }

    /** Starts an ACP agent on stdio: `wizard acp`, or `pi-acp`. */
    fun acp(agent: Agent): String = when (agent) {
        Agent.Wizard -> sh("$PATH_SETUP\nexec wizard acp")
        Agent.Pi -> sh(
            "$PATH_SETUP\n$FIND_PI_ACP\n" +
                "[ -n \"\$piacp\" ] || { echo 'pi-acp is not installed' >&2; exit 127; }\n" +
                "exec \"\$piacp\"",
        )
        Agent.ClaudeCode -> error("Claude Code speaks stream-json, not ACP")
    }

    /**
     * Claude Code for one session, the way Wizard GUI runs it: stream-json
     * both ways, partial messages for streaming, questions on the stdio
     * control channel. `$1` is the working directory, the rest are flags.
     */
    fun claude(cwd: String, flags: List<String>): String =
        sh("$PATH_SETUP\ncd \"\$1\" || exit 3\nshift\n$CLAUDE_AS_ROOT\nexec claude \"\$@\"", cwd, *flags.toTypedArray())

    /** The CLI won't bypass permissions as root outside a sandbox, so there it asks over stdio instead, and the app allows. */
    private val CLAUDE_AS_ROOT = """
        if [ "${'$'}(id -u)" = 0 ] && [ "${'$'}{IS_SANDBOX:-}" != 1 ]; then
          for a do
            shift
            case "${'$'}a" in
              --dangerously-skip-permissions) ;;
              bypassPermissions) set -- "${'$'}@" default ;;
              *) set -- "${'$'}@" "${'$'}a" ;;
            esac
          done
        fi
    """.trimIndent()

    val probe: String = sh(
        """
        $PATH_SETUP
        v() { command -v "${'$'}1" >/dev/null 2>&1 && "${'$'}1" --version 2>/dev/null </dev/null | head -n 1 | tr -d '\r'; }
        printf 'wizard=%s\n' "${'$'}(v wizard)"
        printf 'pi=%s\n' "${'$'}(v pi)"
        $FIND_PI_ACP
        printf 'piacp=%s\n' "${'$'}piacp"
        printf 'claude=%s\n' "${'$'}(v claude)"
        printf 'npm=%s\n' "${'$'}(command -v npm 2>/dev/null)"
        # Agents running in a terminal: wizard minus ACP servers, claude minus print-mode turns (this app's own among them).
        printf 'running=%s\n' "${'$'}(ps -u "${'$'}(id -u)" -o comm= -o args= 2>/dev/null | awk '{ n = ${'$'}1; sub(".*/", "", n); p = " " ${'$'}0 " "; if ((n == "wizard" && ${'$'}NF != "acp") || (n == "claude" && p !~ / (-p|--print) /)) c++ } END { print c + 0 }')"
        printf 'home=%s\n' "${'$'}HOME"
        """.trimIndent(),
    )

    /**
     * Installs an agent the way Wizard GUI's onboarding does. For Pi that is
     * the CLI, then the pinned pi-acp adapter into the same managed directory
     * the desktop app uses, staged and renamed so a killed install never looks
     * finished.
     */
    fun install(agent: Agent): String = when (agent) {
        Agent.Wizard -> sh("$PATH_SETUP\ncurl -fsSL $INSTALL_URL | WIZARD_INSTALL_DIR=\"\$HOME/.local/bin\" bash 2>&1")
        Agent.ClaudeCode -> sh("$PATH_SETUP\ncurl -fsSL https://claude.ai/install.sh | bash 2>&1")
        Agent.Pi -> sh(
            """
            $PATH_SETUP
            exec 2>&1
            if ! command -v pi >/dev/null 2>&1; then
              curl -fsSL https://pi.dev/install.sh | sh || exit 1
              PATH="${'$'}HOME/.local/bin:${'$'}PATH"
            fi
            $FIND_PI_ACP
            [ -n "${'$'}piacp" ] && { echo "pi-acp is already installed"; exit 0; }
            command -v npm >/dev/null 2>&1 || { echo "Pi needs npm for its ACP adapter, and npm isn't on this machine."; exit 1; }
            dir="$PI_ACP_DIR"
            tmp="${'$'}dir.tmp-${'$'}${'$'}"
            rm -rf "${'$'}tmp" && mkdir -p "${'$'}tmp" || exit 1
            echo "Installing pi-acp@$PI_ACP_VERSION"
            npm install --prefix "${'$'}tmp" --no-audit --no-fund --no-progress --loglevel=error --include=optional --cache "${'$'}tmp/.npm-cache" "pi-acp@$PI_ACP_VERSION" || { rm -rf "${'$'}tmp"; exit 1; }
            rm -rf "${'$'}tmp/.npm-cache"
            printf '%s' "$PI_ACP_VERSION" > "${'$'}tmp/.zeron-install-ok"
            rm -rf "${'$'}dir" && mv "${'$'}tmp" "${'$'}dir" && echo "pi-acp installed"
            """.trimIndent(),
        )
    }

    /** Lists the subdirectories of `$1` (`~` expands), one per line as `G\tname` for git repos or `D\tname`. */
    fun listDirs(path: String): String = sh(
        """
        p="${'$'}1"
        case "${'$'}p" in "~") p="${'$'}HOME" ;; "~/"*) p="${'$'}HOME/${'$'}{p#"~/"}" ;; esac
        cd -- "${'$'}p" 2>/dev/null || { echo "no such directory: ${'$'}1" >&2; exit 3; }
        pwd
        for d in */; do
          [ -d "${'$'}d" ] || continue
          n="${'$'}{d%/}"
          if [ -e "${'$'}n/.git" ]; then printf 'G\t%s\n' "${'$'}n"; else printf 'D\t%s\n' "${'$'}n"; fi
        done
        """.trimIndent(),
        path,
    )

    /**
     * Claude Code's saved sessions, newest first: one `S` line per session log
     * under ~/.claude/projects with its mtime, id and cwd, then its title as a
     * JSON string (the AI title if there is one, else the first prompt).
     */
    fun claudeSessions(limit: Int = 60): String = sh(
        """
        cd "${'$'}HOME/.claude/projects" 2>/dev/null || exit 0
        ls -t ./*/*.jsonl 2>/dev/null | head -n "${'$'}1" | while IFS= read -r f; do
          id=${'$'}(basename "${'$'}f" .jsonl)
          m=${'$'}(stat -c %Y "${'$'}f" 2>/dev/null || stat -f %m "${'$'}f" 2>/dev/null || echo 0)
          cwd=${'$'}(grep -m1 -o '"cwd":"[^"]*"' "${'$'}f" | sed 's/^"cwd":"//; s/"${'$'}//')
          t=${'$'}(grep -oE '"(aiTitle|customTitle)":"(\\.|[^"\\])*"' "${'$'}f" | tail -n 1 | sed -E 's/^"[a-zA-Z]+"://')
          [ -n "${'$'}t" ] || t=${'$'}(grep -m1 -oE '"message":\{"role":"user","content":"(\\.|[^"\\])*"' "${'$'}f" | sed 's/^"message":{"role":"user","content"://')
          printf 'S\t%s\t%s\t%s\t%s\n' "${'$'}m" "${'$'}id" "${'$'}cwd" "${'$'}t"
        done
        """.trimIndent(),
        limit.toString(),
    )

    /** The tail of one Claude Code session log. */
    fun claudeTranscript(sessionId: String, lines: Int = 3000): String = sh(
        """
        f=${'$'}(ls "${'$'}HOME"/.claude/projects/*/"${'$'}1".jsonl 2>/dev/null | head -n 1)
        [ -n "${'$'}f" ] || { echo "no saved session ${'$'}1" >&2; exit 3; }
        tail -n "${'$'}2" "${'$'}f"
        """.trimIndent(),
        sessionId,
        lines.toString(),
    )

    data class Probe(
        val agents: Map<Agent, AgentAvailability>,
        val running: Int,
        val home: String?,
    ) {
        /** The Wizard version line, for the status text. */
        val wizardVersion: String? get() = agents[Agent.Wizard]?.version
    }

    fun parseProbe(output: String): Probe {
        val fields = output.lineSequence().mapNotNull { line ->
            val eq = line.indexOf('=')
            if (eq <= 0) null else line.substring(0, eq).trim() to line.substring(eq + 1).trim()
        }.toMap()
        fun field(key: String) = fields[key]?.takeIf { it.isNotEmpty() }
        val pi = field("pi")
        val piAcp = field("piacp")
        val agents = mapOf(
            Agent.Wizard to field("wizard").let { AgentAvailability(it, it != null, null) },
            Agent.Pi to AgentAvailability(
                version = pi,
                ready = pi != null && piAcp != null,
                missing = if (pi != null && piAcp == null) "Needs the pi-acp adapter" else null,
            ),
            Agent.ClaudeCode to field("claude").let { AgentAvailability(it, it != null, null) },
        )
        return Probe(agents, fields["running"]?.toIntOrNull() ?: 0, field("home"))
    }

    data class RemoteDir(val name: String, val isRepo: Boolean)
    data class Listing(val path: String, val dirs: List<RemoteDir>)

    fun parseListing(output: String): Listing {
        val lines = output.lines().filter { it.isNotEmpty() }
        val path = lines.firstOrNull() ?: "/"
        val dirs = lines.drop(1).mapNotNull { line ->
            val tab = line.indexOf('\t')
            if (tab != 1) null else RemoteDir(line.substring(2), line[0] == 'G')
        }.sortedWith(compareBy<RemoteDir> { !it.isRepo }.thenBy { it.name.lowercase() })
        return Listing(path, dirs)
    }
}
