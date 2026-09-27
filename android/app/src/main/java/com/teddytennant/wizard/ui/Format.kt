package com.teddytennant.wizard.ui

import java.time.Duration
import java.time.Instant
import java.time.LocalDateTime
import java.time.OffsetDateTime
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale

object Format {
    /** `2026-08-31T15:39:57.68+00:00`, or a session id like `2026-08-31T15-39-52` (local time). */
    fun instant(value: String?): Instant? {
        if (value.isNullOrBlank()) return null
        runCatching { return OffsetDateTime.parse(value).toInstant() }
        runCatching {
            return LocalDateTime.parse(value, DateTimeFormatter.ofPattern("yyyy-MM-dd'T'HH-mm-ss")).atZone(ZoneId.systemDefault()).toInstant()
        }
        return null
    }

    fun ago(then: Instant?, now: Instant = Instant.now()): String {
        if (then == null) return ""
        val d = Duration.between(then, now)
        return when {
            d.isNegative || d.seconds < 60 -> "just now"
            d.toMinutes() < 60 -> "${d.toMinutes()}m ago"
            d.toHours() < 24 -> "${d.toHours()}h ago"
            d.toDays() < 7 -> "${d.toDays()}d ago"
            else -> DateTimeFormatter.ofPattern("MMM d", Locale.getDefault()).withZone(ZoneId.systemDefault()).format(then)
        }
    }

    fun project(cwd: String): String = cwd.trimEnd('/').substringAfterLast('/').ifEmpty { cwd }

    /** `/home/dev/src/wizard` shown as `~/src/wizard` when home is known. */
    fun path(cwd: String, home: String?): String =
        if (home != null && home.length > 1 && (cwd == home || cwd.startsWith("$home/"))) "~" + cwd.removePrefix(home) else cwd
}
