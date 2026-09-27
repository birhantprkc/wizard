package com.teddytennant.wizard.ssh

/**
 * Shell run on the remote. Every command goes through `sh -c '<script>'` so a
 * login shell of fish or zsh on the other end doesn't change the meaning.
 */
object RemoteScripts {
    /** Where `install.sh` and cargo put wizard, which a non-interactive shell often lacks. */
    private const val PATH_PREFIX = "export PATH=\"\$HOME/.local/bin:\$HOME/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:\$PATH\""

    const val INSTALL_URL = "https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh"

    /** POSIX single quoting; `'\''` also reads correctly in fish. */
    fun quote(value: String): String = "'" + value.replace("'", "'\\''") + "'"

    fun sh(script: String, vararg args: String): String =
        "sh -c " + quote(script) + " sh" + args.joinToString("") { " " + quote(it) }

    val acp: String = sh("$PATH_PREFIX\nexec wizard acp")

    val probe: String = sh(
        """
        $PATH_PREFIX
        if command -v wizard >/dev/null 2>&1; then
          printf 'wizard=%s\n' "${'$'}(wizard --version 2>/dev/null | head -n 1)"
        else
          echo wizard=
        fi
        printf 'running=%s\n' "${'$'}(pgrep -x -u "${'$'}(id -u)" wizard 2>/dev/null | wc -l | tr -d ' ')"
        printf 'home=%s\n' "${'$'}HOME"
        """.trimIndent(),
    )

    /** The desktop app's installer line: into ~/.local/bin, so it never needs sudo. */
    val install: String = sh("curl -fsSL $INSTALL_URL | WIZARD_INSTALL_DIR=\"\$HOME/.local/bin\" bash 2>&1")

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

    data class Probe(val wizardVersion: String?, val running: Int, val home: String?)

    fun parseProbe(output: String): Probe {
        val fields = output.lineSequence().mapNotNull { line ->
            val eq = line.indexOf('=')
            if (eq <= 0) null else line.substring(0, eq).trim() to line.substring(eq + 1).trim()
        }.toMap()
        return Probe(
            wizardVersion = fields["wizard"]?.takeIf { it.isNotEmpty() },
            running = fields["running"]?.toIntOrNull() ?: 0,
            home = fields["home"]?.takeIf { it.isNotEmpty() },
        )
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
