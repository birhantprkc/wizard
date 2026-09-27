package com.teddytennant.wizard.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.teddytennant.wizard.ui.theme.GeistMono
import com.teddytennant.wizard.ui.theme.WizardColors
import com.teddytennant.wizard.ui.theme.WizardTheme

/** The subset of Markdown a coding agent writes. Unclosed fences (mid-stream) still render as code. */
sealed interface MdBlock {
    data class Heading(val level: Int, val text: String) : MdBlock
    data class Paragraph(val text: String) : MdBlock
    data class Code(val language: String?, val code: String) : MdBlock
    data class ListItem(val marker: String, val text: String, val depth: Int) : MdBlock
    data class Quote(val text: String) : MdBlock
    data object Rule : MdBlock
}

object MarkdownParser {
    private val heading = Regex("^(#{1,6})\\s+(.*)$")
    private val bullet = Regex("^(\\s*)([-*+])\\s+(.*)$")
    private val ordered = Regex("^(\\s*)(\\d{1,3})[.)]\\s+(.*)$")
    private val rule = Regex("^\\s*([-*_])(\\s*\\1){2,}\\s*$")

    fun parse(source: String): List<MdBlock> {
        val out = mutableListOf<MdBlock>()
        val para = StringBuilder()
        fun flush() {
            if (para.isNotBlank()) out += MdBlock.Paragraph(para.toString().trim())
            para.clear()
        }
        val lines = source.replace("\r\n", "\n").split('\n')
        var i = 0
        while (i < lines.size) {
            val line = lines[i]
            val trimmed = line.trimStart()
            when {
                trimmed.startsWith("```") || trimmed.startsWith("~~~") -> {
                    flush()
                    val fence = trimmed.take(3)
                    val lang = trimmed.drop(3).trim().ifEmpty { null }
                    val code = StringBuilder()
                    i++
                    while (i < lines.size && !lines[i].trimStart().startsWith(fence)) {
                        if (code.isNotEmpty()) code.append('\n')
                        code.append(lines[i])
                        i++
                    }
                    out += MdBlock.Code(lang, code.toString())
                }
                line.isBlank() -> flush()
                heading.matches(trimmed) -> {
                    flush()
                    val m = heading.find(trimmed)!!
                    out += MdBlock.Heading(m.groupValues[1].length, m.groupValues[2].trim().trimEnd('#').trim())
                }
                rule.matches(line) -> {
                    flush()
                    out += MdBlock.Rule
                }
                bullet.matches(line) -> {
                    flush()
                    val m = bullet.find(line)!!
                    out += MdBlock.ListItem("•", m.groupValues[3], m.groupValues[1].length / 2)
                }
                ordered.matches(line) -> {
                    flush()
                    val m = ordered.find(line)!!
                    out += MdBlock.ListItem(m.groupValues[2] + ".", m.groupValues[3], m.groupValues[1].length / 2)
                }
                trimmed.startsWith(">") -> {
                    flush()
                    out += MdBlock.Quote(trimmed.removePrefix(">").trim())
                }
                else -> {
                    val last = out.lastOrNull()
                    // A wrapped continuation line of a list item.
                    if (para.isEmpty() && last is MdBlock.ListItem && line.startsWith("  ") && i > 0 && lines[i - 1].isNotBlank()) {
                        out[out.lastIndex] = last.copy(text = last.text + " " + trimmed)
                    } else {
                        if (para.isNotEmpty()) para.append('\n')
                        para.append(line)
                    }
                }
            }
            i++
        }
        flush()
        return out
    }

    /** Inline spans: `code`, **bold**, *italic* / _italic_, [text](url). */
    fun inline(text: String, colors: WizardColors): AnnotatedString = buildAnnotatedString {
        var i = 0
        while (i < text.length) {
            val c = text[i]
            when {
                c == '`' -> {
                    val end = text.indexOf('`', i + 1)
                    if (end > i) {
                        withStyle(SpanStyle(fontFamily = GeistMono, fontSize = 13.5.sp, background = colors.raised, color = colors.text)) {
                            append(" " + text.substring(i + 1, end) + " ")
                        }
                        i = end + 1
                    } else {
                        append(c); i++
                    }
                }
                text.startsWith("**", i) || text.startsWith("__", i) -> {
                    val mark = text.substring(i, i + 2)
                    val end = text.indexOf(mark, i + 2)
                    if (end > i + 2) {
                        withStyle(SpanStyle(fontWeight = FontWeight.SemiBold)) { append(inline(text.substring(i + 2, end), colors)) }
                        i = end + 2
                    } else {
                        append(mark); i += 2
                    }
                }
                (c == '*' || c == '_') && i + 1 < text.length && !text[i + 1].isWhitespace() &&
                    (i == 0 || !text[i - 1].isLetterOrDigit()) -> {
                    val end = text.indexOf(c, i + 1)
                    if (end > i + 1 && !text[end - 1].isWhitespace()) {
                        withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { append(text.substring(i + 1, end)) }
                        i = end + 1
                    } else {
                        append(c); i++
                    }
                }
                c == '[' -> {
                    val close = text.indexOf("](", i)
                    val end = if (close > i) text.indexOf(')', close) else -1
                    if (close > i && end > close) {
                        val label = text.substring(i + 1, close)
                        val url = text.substring(close + 2, end)
                        withLink(LinkAnnotation.Url(url, TextLinkStyles(SpanStyle(color = colors.accent, textDecoration = TextDecoration.Underline)))) {
                            append(label)
                        }
                        i = end + 1
                    } else {
                        append(c); i++
                    }
                }
                else -> {
                    append(c); i++
                }
            }
        }
    }
}

@Composable
fun Markdown(text: String, modifier: Modifier = Modifier) {
    val colors = WizardTheme.colors
    val type = WizardTheme.type
    val blocks = remember(text) { MarkdownParser.parse(text) }
    Column(modifier, verticalArrangement = Arrangement.spacedBy(10.dp)) {
        blocks.forEach { block ->
            when (block) {
                is MdBlock.Heading -> Text(
                    MarkdownParser.inline(block.text, colors),
                    style = if (block.level <= 2) type.heading.copy(fontSize = 17.sp) else type.heading,
                    color = colors.text,
                    modifier = Modifier.padding(top = 4.dp),
                )
                is MdBlock.Paragraph -> Text(MarkdownParser.inline(block.text, colors), style = type.body, color = colors.text)
                is MdBlock.Code -> CodeBox(block.code)
                is MdBlock.ListItem -> Row(Modifier.padding(start = (block.depth * 16).dp)) {
                    Text(block.marker, style = type.body, color = colors.faint, modifier = Modifier.widthIn(min = 18.dp))
                    Text(MarkdownParser.inline(block.text, colors), style = type.body, color = colors.text)
                }
                is MdBlock.Quote -> Row(Modifier.height(IntrinsicSize.Min)) {
                    Box(Modifier.width(2.dp).fillMaxHeight().background(colors.borderStrong))
                    Text(MarkdownParser.inline(block.text, colors), style = type.body, color = colors.muted, modifier = Modifier.padding(start = 12.dp))
                }
                MdBlock.Rule -> Box(Modifier.fillMaxWidth().padding(vertical = 6.dp).height(1.dp).background(colors.border))
            }
        }
    }
}
