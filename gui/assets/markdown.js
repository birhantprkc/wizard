// Wizard GUI — the markdown renderer: block parse, inline parse, blocks to
// DOM, and the streaming view that repaints only what a delta changed.

import { h } from './dom.js';

/* ------------------------------------------------------------------------ */
/* Markdown                                                                  */
/* ------------------------------------------------------------------------ */

/* The one markdown renderer: assistant messages, subagent messages, plan and
   interview cards all come through here.

   Injection-safe by construction. Every element is built with `h()` and every
   scrap of model text lands in a text node — `innerHTML` is never handed model
   output — so a reply containing `<script>alert(1)</script>` renders as those
   characters. Link targets are model output too, and are checked against a
   scheme allowlist before they are ever put in an `href`.

   The parse is block-first, and every block keeps the exact source lines it came
   from (`.src`). That is what makes the streaming path cheap: a delta re-parses
   the message, but only the blocks whose source actually changed get redrawn.
   See `syncMarkdown`. */

/** ``` or ~~~, up to three spaces in, with an optional info string (the language). */
const FENCE_RE = /^ {0,3}(`{3,}|~{3,})[ \t]*(\S*)/;
const HEADING_RE = /^ {0,3}(#{1,6})[ \t]+(.*?)[ \t]*#*[ \t]*$/;
const HR_RE = /^ {0,3}(?:-{3,}|\*{3,}|_{3,})[ \t]*$/;
const QUOTE_RE = /^ {0,3}> ?(.*)$/;
/** A bullet (`-`, `*`, `+`) or a number (`1.`, `1)`), and what follows it. */
const ITEM_RE = /^([ \t]*)(?:([-*+])|(\d{1,9})[.)])(?:[ \t]+(.*))?$/;
/** Underlined heading: `===` (h1) or `---` (h2) under the paragraph it titles. */
const SETEXT_RE = /^ {0,3}(=+|-{3,})[ \t]*$/;
/** `- [ ]` / `- [x]`: a checklist item, which a model writes constantly. */
const TASK_RE = /^\[([ xX])\][ \t]+/;

/** Leading whitespace in columns, a tab being four of them. */
function indentOf(line) {
  let cols = 0;
  for (const ch of line) {
    if (ch === ' ') cols += 1;
    else if (ch === '\t') cols += 4;
    else break;
  }
  return cols;
}

/** Drop up to `cols` columns of leading whitespace. */
function dedent(line, cols) {
  let i = 0;
  let col = 0;
  while (i < line.length && col < cols) {
    if (line[i] === ' ') col += 1;
    else if (line[i] === '\t') col += 4;
    else break;
    i += 1;
  }
  return line.slice(i);
}

/** Which characters of a row sit inside an inline code span. A pipe in there is
 *  content — `` `a | b` `` is one cell, not two — and a backtick run that never
 *  closes on the line opens nothing, so a stray backtick cannot eat the row. */
function codeMask(s) {
  const mask = new Array(s.length).fill(false);
  let i = 0;
  while (i < s.length) {
    if (s[i] !== '`') {
      i += 1;
      continue;
    }
    let open = 1;
    while (s[i + open] === '`') open += 1;
    // The span closes on the next run of backticks of exactly the same length.
    let j = i + open;
    while (j < s.length) {
      if (s[j] !== '`') {
        j += 1;
        continue;
      }
      let run = 1;
      while (s[j + run] === '`') run += 1;
      if (run === open) break;
      j += run;
    }
    if (j >= s.length) {
      i += open; // never closed: those backticks are literal text
      continue;
    }
    for (let k = i; k < j + open; k += 1) mask[k] = true;
    i = j + open;
  }
  return mask;
}

/** A table row's cells: split on unescaped pipes outside code spans, minus the
 *  optional fencing pipes at either end, which delimit rather than open a cell. */
function splitRow(line) {
  const s = line.trim();
  const code = codeMask(s);
  const cells = [];
  let cur = '';
  for (let i = 0; i < s.length; i += 1) {
    if (s[i] === '\\' && s[i + 1] === '|') {
      // GFM: in a table an escaped pipe is a pipe, even inside a code span.
      cur += '|';
      i += 1;
    } else if (s[i] === '|' && !code[i]) {
      cells.push(cur);
      cur = '';
    } else {
      cur += s[i];
    }
  }
  cells.push(cur);
  if (cells.length > 1 && s.startsWith('|') && !cells[0].trim()) cells.shift();
  if (cells.length > 1 && s.endsWith('|') && !cells[cells.length - 1].trim()) cells.pop();
  return cells.map((c) => c.trim());
}

/** The row of dashes under a header is what makes a pipe table a table. */
function isAlignRow(line) {
  if (!line.includes('-') || !/^[\s|:-]+$/.test(line)) return false;
  const cells = splitRow(line);
  return cells.length > 0 && cells.every((c) => /^:?-+:?$/.test(c));
}

/** `:--` left, `--:` right, `:-:` center, `---` unset. */
function cellAlign(spec) {
  const left = spec.startsWith(':');
  const right = spec.endsWith(':');
  if (left && right) return 'center';
  if (right) return 'right';
  if (left) return 'left';
  return '';
}

/** A pipe table opens on a header row followed by an alignment row with the same
 *  number of cells — GFM's own rule, and the one that keeps a sentence which
 *  happens to hold a `|` above a `---` from turning into a one-column table.
 *  @returns {?{head: string[], align: string[]}} */
function tableAt(lines, i) {
  if (!lines[i].includes('|') || i + 1 >= lines.length || !isAlignRow(lines[i + 1])) return null;
  const head = splitRow(lines[i]);
  const align = splitRow(lines[i + 1]);
  if (head.length !== align.length) return null;
  return { head, align: align.map(cellAlign) };
}

/** True if a line would open some block other than a paragraph — i.e. it ends
 *  the paragraph above it even without a blank line between them. */
function startsBlock(lines, i) {
  const line = lines[i];
  if (FENCE_RE.test(line) || HEADING_RE.test(line) || HR_RE.test(line)) return true;
  if (QUOTE_RE.test(line) || ITEM_RE.test(line)) return true;
  return tableAt(lines, i) !== null;
}

/**
 * Markdown source → blocks. Each block carries the exact source it was parsed
 * from, so two parses can be diffed against each other cheaply.
 * @param {string} md
 * @returns {Array<Object>}
 */
function parseBlocks(md) {
  const lines = String(md).replace(/\r\n?/g, '\n').split('\n');
  const blocks = [];
  let i = 0;
  const since = (from) => lines.slice(from, i).join('\n');

  while (i < lines.length) {
    if (!lines[i].trim()) { i += 1; continue; } // blank lines only separate blocks
    const start = i;
    const line = lines[i];

    // Four columns in, at the top of a block, is a code block that was written
    // without a fence. The blank lines inside one belong to it; the ones after
    // it do not, so the run is cut back to the last line that held code.
    if (indentOf(line) >= 4) {
      let end = i;
      while (i < lines.length && (!lines[i].trim() || indentOf(lines[i]) >= 4)) {
        if (lines[i].trim()) end = i;
        i += 1;
      }
      i = end + 1;
      const code = lines.slice(start, i).map((l) => dedent(l, 4)).join('\n');
      blocks.push({ kind: 'code', lang: '', code, src: since(start) });
      continue;
    }

    const fence = FENCE_RE.exec(line);
    if (fence) {
      // Closed by a fence of the same character, at least as long as the opener.
      const close = new RegExp(`^ {0,3}${fence[1][0] === '`' ? '`' : '~'}{${fence[1].length},}[ \t]*$`);
      const body = [];
      i += 1;
      while (i < lines.length && !close.test(lines[i])) body.push(lines[i++]);
      if (i < lines.length) i += 1; // the closing fence — absent mid-stream
      blocks.push({ kind: 'code', lang: fence[2], code: body.join('\n'), src: since(start) });
      continue;
    }

    const heading = HEADING_RE.exec(line);
    if (heading) {
      i += 1;
      blocks.push({ kind: 'heading', level: heading[1].length, text: heading[2], src: since(start) });
      continue;
    }

    if (HR_RE.test(line)) {
      i += 1;
      blocks.push({ kind: 'hr', src: since(start) });
      continue;
    }

    if (QUOTE_RE.test(line)) {
      const inner = [];
      while (i < lines.length && lines[i].trim()) {
        const m = QUOTE_RE.exec(lines[i]);
        // An unmarked line under a quoted paragraph is a lazy continuation and
        // still belongs to the quote — but a line that opens a block of its own
        // (a heading, a fence, a list, a rule) ends the quote instead of being
        // swallowed by it.
        if (!m && startsBlock(lines, i)) break;
        inner.push(m ? m[1] : lines[i]);
        i += 1;
      }
      // The quote's content is markdown in its own right.
      blocks.push({ kind: 'quote', inner: inner.join('\n'), src: since(start) });
      continue;
    }

    const table = tableAt(lines, i);
    if (table) {
      const rows = [];
      i += 2;
      while (i < lines.length && lines[i].trim() && lines[i].includes('|') && !isAlignRow(lines[i])) {
        rows.push(splitRow(lines[i]));
        i += 1;
      }
      blocks.push({ kind: 'table', ...table, rows, src: since(start) });
      continue;
    }

    if (ITEM_RE.test(line)) {
      const base = indentOf(line);
      const numbered = !ITEM_RE.exec(line)[2];
      // Switching marker — a `-` list under a `1.` list — starts a new list, not
      // another item of this one.
      const switched = (l) => {
        const m = ITEM_RE.exec(l);
        return m && indentOf(l) <= base && !m[2] !== numbered;
      };
      while (i < lines.length) {
        const l = lines[i];
        if (!l.trim()) {
          // A blank line ends the list unless the list carries on beneath it.
          const next = lines[i + 1];
          if (!next || !next.trim()) break;
          if (!ITEM_RE.test(next) && indentOf(next) <= base) break;
          if (switched(next)) break;
          i += 1;
          continue;
        }
        if (switched(l)) break;
        const item = ITEM_RE.exec(l);
        if (item ? indentOf(l) < base : indentOf(l) <= base) break;
        i += 1;
      }
      blocks.push({ kind: 'list', ...parseList(lines.slice(start, i)), src: since(start) });
      continue;
    }

    const para = [];
    let setext = null;
    while (i < lines.length && lines[i].trim()) {
      if (para.length) {
        // An underline turns the lines above it into a heading. It is checked
        // before the block starters because `---` is also a rule: under a
        // paragraph it underlines it, and only on its own is it a rule.
        setext = SETEXT_RE.exec(lines[i]);
        if (setext) {
          i += 1;
          break;
        }
        if (startsBlock(lines, i)) break;
      }
      para.push(lines[i]);
      i += 1;
    }
    if (setext) {
      const level = setext[1][0] === '=' ? 1 : 2;
      blocks.push({ kind: 'heading', level, text: para.join('\n'), src: since(start) });
      continue;
    }
    blocks.push({ kind: 'para', text: para.join('\n'), src: since(start) });
  }
  return blocks;
}

/**
 * A list block's items. An item's body is everything under its marker —
 * continuation lines and nested lists alike — dedented to the marker's content
 * column, which makes it a little markdown document of its own. Rendering
 * recurses into it, and that is how nesting (and a code block inside a bullet)
 * comes out right.
 */
function parseList(lines) {
  const first = ITEM_RE.exec(lines[0]);
  const ordered = !first[2];
  const base = indentOf(lines[0]);
  const items = [];
  let cur = null;
  let content = 0;
  for (const line of lines) {
    const m = ITEM_RE.exec(line);
    if (m && indentOf(line) <= base) {
      cur = [m[4] || ''];
      items.push(cur);
      content = indentOf(line) + (m[2] ? m[2].length : m[3].length + 1) + 1;
      continue;
    }
    if (cur) cur.push(dedent(line, content));
  }
  return { ordered, start: ordered ? Number(first[3]) : 1, items: items.map((l) => l.join('\n')) };
}

/* --- Inline --------------------------------------------------------------- */

const ESCAPABLE = /[\\`*_{}[\]()#+\-.!|~<>]/;
const CODE_SPAN = /^(`+)([\s\S]*?)\1(?!`)/;
const STRONG_EM_STAR = /^\*\*\*(?=\S)([\s\S]*?\S)\*\*\*/;
const STRONG_EM_UNDER = /^___(?=\S)([\s\S]*?\S)___(?!\w)/;
const STRONG_STAR = /^\*\*(?=\S)([\s\S]*?\S)\*\*/;
/** An underscore only closes emphasis where a word does not carry on through it:
 *  `__init__` is bold, but `_private_var` is a name and stays one. The content of
 *  an underscore span stops at the next run of the same delimiter, so an opener
 *  that never closes cannot reach across the rest of the line to find one. */
const STRONG_UNDER = /^__(?=\S)((?:[^_]|_(?!_))*?\S)__(?!\w)/;
const EM_STAR = /^\*(?=\S)([\s\S]*?\S)\*(?!\*)/;
const EM_UNDER = /^_(?=\S)([^_]*?\S)_(?!\w)/;
const STRIKE = /^~~(?=\S)([\s\S]*?\S)~~/;
/** `[text](url)`, or `![alt](url)`. The URL may carry balanced parens. */
const LINK = /^(!?)\[((?:\\.|[^\][\\])*)\]\([ \t]*((?:[^\s()\\]|\\.|\([^\s()]*\))*)(?:[ \t]+"([^"]*)")?[ \t]*\)/;
/** A bare URL in prose. It stops before trailing punctuation, which is a
 *  sentence's, not the URL's. */
const AUTOLINK = /^https?:\/\/[^\s<>[\]()"'`]+[^\s<>[\]()"'`.,;:!?]/;
/** `<https://…>`: a URL the model bracketed rather than left bare. */
const ANGLE_LINK = /^<([a-z][a-z\d+.-]*:[^\s<>]+)>/i;

/** The schemes we will hand a browser. Anything else — `javascript:`, `data:`,
 *  `vbscript:` — is not a link, and its source text is shown instead. */
const SAFE_SCHEME = /^(?:https?:\/\/|mailto:|tel:)/i;

/**
 * A link target out of a model, or null if it is not one we will follow.
 * @param {string} raw
 * @returns {string|null}
 */
function safeHref(raw) {
  const url = raw.trim().replace(/^<([\s\S]*)>$/, '$1');
  // `java&#9;script:` and friends: a browser strips control characters before
  // resolving the scheme, so the check has to look at what it will actually see.
  const probe = url.replace(/[\u0000-\u0020\u00a0]/g, '');
  return SAFE_SCHEME.test(probe) ? url : null;
}

/**
 * Inline markdown, in one left-to-right pass. Code spans win over everything
 * (their content is literal), then links, then emphasis.
 * @param {string} text
 * @param {boolean} [inLink] inside an `<a>` already: no nested links
 * @returns {Array<Node|string>} children for the enclosing block
 */
function inlineNodes(text, inLink = false) {
  const src = String(text);
  const out = [];
  let buf = '';
  const flush = () => {
    if (buf) out.push(buf);
    buf = '';
  };
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const rest = src.slice(i);

    // A backslash escape is the author saying "this one is a literal".
    if (c === '\\' && ESCAPABLE.test(src[i + 1] || '')) {
      buf += src[i + 1];
      i += 2;
      continue;
    }

    // A newline inside a paragraph is a line break. CommonMark would make it a
    // space, but a model that writes lines means lines — collapsing them into
    // one run-on paragraph is the bug this renderer exists to kill.
    if (c === '\n') {
      buf = buf.replace(/[ \t]+$/, ''); // the spaces that ended the line are not content
      flush();
      out.push(h('br'));
      i += 1;
      continue;
    }

    if (c === '`') {
      const m = CODE_SPAN.exec(rest);
      if (m && m[2]) {
        flush();
        // One padding space either side is a fence for backticks, not content.
        const body = /^ .* $/.test(m[2]) ? m[2].slice(1, -1) : m[2];
        out.push(h('code', { class: 'md-code' }, body));
        i += m[0].length;
        continue;
      }
    }

    if (!inLink && (c === '[' || (c === '!' && src[i + 1] === '['))) {
      const m = LINK.exec(rest);
      if (m) {
        flush();
        const href = safeHref(m[3]);
        if (href) {
          // Model output: it opens in its own tab and cannot reach back into ours.
          // An image (`![alt](…)`) links rather than loads — a transcript does not
          // fetch from wherever a model points it.
          out.push(h('a', {
            class: 'md-link', href, target: '_blank', rel: 'noopener noreferrer',
            title: m[4] || null,
          }, ...inlineNodes(m[2], true)));
        } else {
          out.push(m[0]); // not a scheme we will follow: it says what it says
        }
        i += m[0].length;
        continue;
      }
    }

    if (!inLink && c === '<') {
      const m = ANGLE_LINK.exec(rest);
      const href = m && safeHref(m[1]);
      if (href) {
        flush();
        out.push(h('a', { class: 'md-link', href, target: '_blank', rel: 'noopener noreferrer' }, m[1]));
        i += m[0].length;
        continue;
      }
      // Anything else in angle brackets — `<div>`, `<script>`, a scheme we will
      // not follow — is text, and falls through to the buffer as it stands.
    }

    if (!inLink && c === 'h' && AUTOLINK.test(rest)) {
      const url = AUTOLINK.exec(rest)[0];
      flush();
      out.push(h('a', { class: 'md-link', href: url, target: '_blank', rel: 'noopener noreferrer' }, url));
      i += url.length;
      continue;
    }

    if (c === '~') {
      const m = STRIKE.exec(rest);
      if (m) {
        flush();
        out.push(h('del', {}, ...inlineNodes(m[1], inLink)));
        i += m[0].length;
        continue;
      }
    }

    if (c === '*' || c === '_') {
      // `snake_case` is a word, not emphasis: an underscore mid-word is a character.
      const intraword = c === '_' && /\w/.test(src[i - 1] || '');
      if (!intraword) {
        const both = (c === '*' ? STRONG_EM_STAR : STRONG_EM_UNDER).exec(rest);
        if (both) {
          flush();
          out.push(h('strong', {}, h('em', {}, ...inlineNodes(both[1], inLink))));
          i += both[0].length;
          continue;
        }
        const strong = (c === '*' ? STRONG_STAR : STRONG_UNDER).exec(rest);
        if (strong) {
          flush();
          out.push(h('strong', {}, ...inlineNodes(strong[1], inLink)));
          i += strong[0].length;
          continue;
        }
        const em = (c === '*' ? EM_STAR : EM_UNDER).exec(rest);
        if (em) {
          flush();
          out.push(h('em', {}, ...inlineNodes(em[1], inLink)));
          i += em[0].length;
          continue;
        }
      }
    }

    buf += c;
    i += 1;
  }
  flush();
  return out;
}

/* --- Blocks → DOM --------------------------------------------------------- */

/** A fenced block. The info string is labelled rather than dropped: it is the
 *  one part of a code block that says what you are looking at. */
function renderCode(block) {
  const pre = h('pre', { class: 'md-pre' }, h('code', {}, block.code));
  if (!block.lang) return pre;
  return h('div', { class: 'md-code-block' },
    h('div', { class: 'md-code-lang mono' }, block.lang),
    pre);
}

/** A GFM pipe table. It scrolls inside its own box — a wide table must not push
 *  the transcript column sideways. Cells past the header's width are dropped and
 *  missing ones filled, which is what the alignment row promised. */
function renderTable(block) {
  const cell = (tag, text, i) =>
    h(tag, { class: block.align[i] ? `md-al-${block.align[i]}` : null }, ...inlineNodes(text || ''));
  return h('div', { class: 'md-table-wrap' },
    h('table', { class: 'md-table' },
      h('thead', {}, h('tr', {}, ...block.head.map((c, i) => cell('th', c, i)))),
      h('tbody', {}, ...block.rows.map((row) =>
        h('tr', {}, ...block.head.map((_, i) => cell('td', row[i], i)))))));
}

/** One list item. Its body is markdown, so it can hold a nested list or a code
 *  block; a plain one is put straight into the `<li>` rather than wrapped in a
 *  `<p>` that would space the whole list out. An item the model wrote as a
 *  checklist gets a checkbox rather than the `[ ]` it typed — read-only, because
 *  it is a transcript of what the model said, not a form. */
function renderListItem(src) {
  const li = h('li');
  const blocks = parseBlocks(src);
  if (blocks.length && blocks[0].kind === 'para') {
    let text = blocks.shift().text;
    const task = TASK_RE.exec(text);
    if (task) {
      li.className = 'md-task';
      li.append(h('input', {
        class: 'md-check', type: 'checkbox', disabled: true,
        checked: task[1] !== ' ', 'aria-hidden': 'true',
      }));
      text = text.slice(task[0].length);
    }
    li.append(...inlineNodes(text));
  }
  for (const block of blocks) li.append(renderBlock(block));
  return li;
}

function renderBlock(block) {
  switch (block.kind) {
    case 'heading':
      return h(`h${block.level}`, {}, ...inlineNodes(block.text));
    case 'hr':
      return h('hr');
    case 'code':
      return renderCode(block);
    case 'table':
      return renderTable(block);
    case 'quote': {
      const quote = h('blockquote');
      for (const inner of parseBlocks(block.inner)) quote.append(renderBlock(inner));
      return quote;
    }
    case 'list': {
      const list = h(block.ordered ? 'ol' : 'ul', { start: block.start !== 1 ? String(block.start) : null });
      for (const item of block.items) list.append(renderListItem(item));
      return list;
    }
    default:
      return h('p', {}, ...inlineNodes(block.text));
  }
}

/**
 * Render markdown into `root`, replacing whatever was there. `root` must carry
 * the `md` class for the stylesheet to reach it.
 * @param {HTMLElement} root
 * @param {string} md
 */
export function renderMarkdownInto(root, md) {
  root.replaceChildren(...parseBlocks(md).map(renderBlock));
}

/* --- Streaming ------------------------------------------------------------ */

/**
 * A message still being written: the markdown source received so far, and the
 * blocks currently on screen. A delta re-renders only the blocks whose source
 * changed — in a stream that is the last one — so finished paragraphs, tables
 * and code blocks keep the very same DOM nodes, and a selection inside them
 * survives the next token.
 * @param {HTMLElement} target where the message goes
 * @param {string} cls the root's classes
 */
export function newMarkdownView(target, cls) {
  const view = { root: h('div', { class: cls }), src: '', blocks: [], frame: 0, after: null };
  target.append(view.root);
  return view;
}

/** Take a delta. The repaint is one per animation frame, not one per token. */
export function pushMarkdown(view, delta, after) {
  view.src += delta;
  view.after = after;
  if (view.frame) return;
  view.frame = requestAnimationFrame(() => {
    view.frame = 0;
    syncMarkdown(view);
    if (view.after) view.after();
  });
}

/** A table row that is not part of a table yet: it opens or closes with a pipe,
 *  the way a row does and a sentence does not. */
const PIPE_ROW = /^ {0,3}\||\|[ \t]*$/;

/**
 * The source to draw while the rest of it is still coming. A table arrives header
 * first, and until its alignment row lands there is nothing to say it is a table —
 * so drawing it as it stands means a paragraph of pipes that a token later is
 * thrown away and replaced by the table. Hold those trailing rows back instead:
 * they land on the next sync, as the table they became or as the prose they turned
 * out to be, and the last sync of all (`endMarkdown`) holds nothing. Rows inside a
 * table or a code fence are not a trailing paragraph, and are never held.
 * @param {string} src
 * @param {Array<Object>} blocks `src` already parsed
 * @returns {?string} the source minus the held rows, or null if there are none
 */
function withoutNascentTable(src, blocks) {
  const last = blocks[blocks.length - 1];
  if (!last || last.kind !== 'para') return null;
  const at = src.lastIndexOf(last.src);
  // Anything after the paragraph but the cursor — a blank line — means no
  // alignment row is coming, and the pipes are prose after all.
  const tail = src.slice(at + last.src.length);
  if (tail !== '' && tail !== '\n') return null;
  const lines = last.src.split('\n');
  let cut = lines.length;
  while (cut > 0 && PIPE_ROW.test(lines[cut - 1])) cut -= 1;
  if (cut === lines.length) return null;
  return src.slice(0, at) + lines.slice(0, cut).join('\n');
}

/** Reconcile the DOM with the source: keep the leading blocks whose source has
 *  not changed, redraw from the first that has. */
function syncMarkdown(view, final = false) {
  let next = parseBlocks(view.src);
  if (!final) {
    const held = withoutNascentTable(view.src, next);
    if (held !== null) next = parseBlocks(held);
  }
  const prev = view.blocks;
  let keep = 0;
  while (keep < next.length && keep < prev.length && next[keep].src === prev[keep].src) {
    next[keep].node = prev[keep].node; // untouched: its DOM node, and any selection in it, stands
    keep += 1;
  }
  for (let i = keep; i < prev.length; i += 1) prev[i].node.remove();
  for (let i = keep; i < next.length; i += 1) {
    next[i].node = renderBlock(next[i]);
    view.root.append(next[i].node);
  }
  view.blocks = next;
}

/** The message is complete: land the last delta — every line of it — and stop. */
export function endMarkdown(view) {
  if (view.frame) cancelAnimationFrame(view.frame);
  view.frame = 0;
  syncMarkdown(view, true);
  view.root.classList.remove('streaming');
}
