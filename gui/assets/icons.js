// Wizard GUI — inline SVG icon set.
// Every icon is a self-contained SVG string (stroke = currentColor) so the
// bundle needs no external fonts, sprites, or CDN fetches. Sized via CSS.

/**
 * Wrap SVG inner markup in a standard 24x24 stroke-based frame.
 * @param {string} inner
 * @param {string} [extra] extra attributes for the <svg> element
 * @returns {string}
 */
const s = (inner, extra = '') =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"${extra ? ' ' + extra : ''}>${inner}</svg>`;

export const icons = {
  // Sidebar actions
  plusSquare: s('<rect x="3" y="3" width="18" height="18" rx="4.5"/><path d="M12 8v8"/><path d="M8 12h8"/>'),
  folder: s('<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>'),
  wand: s('<path d="M15 4V2"/><path d="M15 16v-2"/><path d="M8 9h2"/><path d="M20 9h2"/><path d="M17.8 11.8 19 13"/><path d="M15 9h.01"/><path d="M17.8 6.2 19 5"/><path d="m3 21 9-9"/><path d="M12.2 6.2 11 5"/>'),
  filter: s('<rect x="3" y="4" width="18" height="5" rx="1.5"/><path d="M5 9v8.5A2.5 2.5 0 0 0 7.5 20h9a2.5 2.5 0 0 0 2.5-2.5V9"/><path d="M10 13h4"/>'),

  // Git
  branch: s('<path d="M6 3v12"/><circle cx="18" cy="6" r="2.7"/><circle cx="6" cy="18" r="2.7"/><path d="M18 9a9 9 0 0 1-9 9"/>'),
  commitNode: s('<circle cx="12" cy="12" r="3"/><path d="M3 12h6"/><path d="M15 12h6"/>'),
  diff: s('<rect x="3" y="3" width="18" height="18" rx="4.5"/><path d="M9.5 7.3v4.4"/><path d="M7.3 9.5h4.4"/><path d="M12.3 15.7h4.4"/>'),

  // Context panel
  target: s('<circle cx="12" cy="12" r="9.2"/><circle cx="12" cy="12" r="5"/><circle cx="12" cy="12" r="1.1" fill="currentColor" stroke="none"/>'),
  checkCircle: s('<circle cx="12" cy="12" r="9"/><path d="m8.4 12.4 2.4 2.4 4.8-5.4"/>'),
  circle: s('<circle cx="12" cy="12" r="9"/>'),
  circleDot: s('<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="3" fill="currentColor" stroke="none"/>'),

  // Tool rows / cards
  globe: s('<circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3a13.4 13.4 0 0 1 3.6 9 13.4 13.4 0 0 1-3.6 9 13.4 13.4 0 0 1-3.6-9A13.4 13.4 0 0 1 12 3"/>'),
  agents: s('<circle cx="9" cy="8" r="3.2"/><path d="M3.5 20a5.5 5.5 0 0 1 11 0"/><circle cx="17.5" cy="9.5" r="2.4"/><path d="M14.9 15.6a4.4 4.4 0 0 1 6.6 3.8"/>'),
  clipboard: s('<rect x="5" y="4" width="14" height="17" rx="2.5"/><path d="M9 4.5V3.5A1.5 1.5 0 0 1 10.5 2h3A1.5 1.5 0 0 1 15 3.5v1"/><path d="M8.5 10h7"/><path d="M8.5 13.5h7"/><path d="M8.5 17h4"/>'),
  question: s('<circle cx="12" cy="12" r="9"/><path d="M9.3 9.2a2.8 2.8 0 0 1 5.4 1c0 1.8-2.7 2.3-2.7 3.8"/><circle cx="12" cy="17.2" r=".4" fill="currentColor"/>'),
  check: s('<path d="m5 12.5 4.5 4.5L19 7.5"/>'),
  close: s('<path d="M6 6l12 12"/><path d="M18 6 6 18"/>'),

  // Chrome / misc
  gear: s('<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>'),
  magnifier: s('<circle cx="11" cy="11" r="7"/><path d="m20 20-3.6-3.6"/>'),
  pencil: s('<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/><path d="m15 5 4 4"/>'),
  sendArrow: s('<path d="M12 19V5"/><path d="m6 11 6-6 6 6"/>', 'stroke-width="2"'),
  hand: s('<path d="M18 11V6a2 2 0 0 0-2-2a2 2 0 0 0-2 2"/><path d="M14 10V4a2 2 0 0 0-2-2a2 2 0 0 0-2 2v2"/><path d="M10 10.5V6a2 2 0 0 0-2-2a2 2 0 0 0-2 2v8"/><path d="M18 8a2 2 0 1 1 4 0v6a8 8 0 0 1-8 8h-2c-2.8 0-4.5-.86-5.99-2.34l-3.6-3.6a2 2 0 0 1 2.83-2.82L7 15"/>'),
  chevronDown: s('<path d="m6 9 6 6 6-6"/>'),
  ellipsis: s('<g fill="currentColor" stroke="none"><circle cx="5" cy="12" r="1.6"/><circle cx="12" cy="12" r="1.6"/><circle cx="19" cy="12" r="1.6"/></g>'),
  plus: s('<path d="M12 5v14"/><path d="M5 12h14"/>'),
  arrowLeft: s('<path d="M19 12H5"/><path d="m11 18-6-6 6-6"/>'),
  arrowRight: s('<path d="M5 12h14"/><path d="m13 6 6 6-6 6"/>'),
  panelLeft: s('<rect x="3" y="4" width="18" height="16" rx="3.5"/><path d="M9.5 4.5v15"/>'),
  panelRight: s('<rect x="3" y="4" width="18" height="16" rx="3.5"/><path d="M14.5 4.5v15"/>'),
  notesPanel: s('<rect x="3" y="4" width="18" height="16" rx="3.5"/><path d="M7.5 9h9"/><path d="M7.5 12.5h9"/><path d="M7.5 16h5"/>'),
  terminal: s('<rect x="3" y="4" width="18" height="16" rx="3.5"/><path d="m7.5 9.5 3 2.7-3 2.7"/><path d="M12.7 15.6H16"/>'),
  spinner: s('<path d="M12 3a9 9 0 1 0 9 9"/>'),
  gauge: s('<path d="m12 14 4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/>'),
  sparkles: s('<path d="M10 3.6 11.6 7.9 15.9 9.5 11.6 11.1 10 15.4 8.4 11.1 4.1 9.5 8.4 7.9Z" fill="currentColor" stroke-width="1.2"/><path d="m17.6 13.4.8 2.1 2.1.8-2.1.8-.8 2.1-.8-2.1-2.1-.8 2.1-.8Z" fill="currentColor" stroke-width="1"/>'),
};

/** Colored filetype chip glyphs, keyed by extension. */
const FILE_STYLES = {
  html: { bg: '#e0653a', fg: '#ffffff', label: '&lt;&gt;', size: 6.2 },
  js: { bg: '#e8d44d', fg: '#1c1c1c', label: 'JS', size: 6.6 },
  mjs: { bg: '#e8d44d', fg: '#1c1c1c', label: 'JS', size: 6.6 },
  ts: { bg: '#3178c6', fg: '#ffffff', label: 'TS', size: 6.6 },
  css: { bg: '#7c5cd6', fg: '#ffffff', label: '#', size: 8.5 },
  rs: { bg: '#c96f42', fg: '#ffffff', label: 'R', size: 8 },
  toml: { bg: '#5a6270', fg: '#ffffff', label: 'T', size: 8 },
  md: { bg: '#4b5563', fg: '#ffffff', label: 'M', size: 8 },
  json: { bg: '#5a6270', fg: '#ffffff', label: '{}', size: 6.2 },
};

/**
 * Small colored file-type icon used in "Wrote" file chips.
 * @param {string} name file name (extension picks the glyph)
 * @returns {string} SVG string
 */
export function fileIconSvg(name) {
  const ext = String(name).split('.').pop().toLowerCase();
  const st = FILE_STYLES[ext] || { bg: '#5a6270', fg: '#ffffff', label: '·', size: 9 };
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">` +
    `<rect x="1" y="1" width="14" height="14" rx="3.6" fill="${st.bg}"/>` +
    `<text x="8" y="11.2" text-anchor="middle" font-family="ui-monospace, SFMono-Regular, Menlo, monospace" font-size="${st.size}" font-weight="700" fill="${st.fg}">${st.label}</text>` +
    `</svg>`
  );
}
