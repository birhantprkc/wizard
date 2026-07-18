// Wizard GUI — overlays: Settings and first-run onboarding.

import { $, h, iconBtn, fillWith } from './dom.js';
import { updateModelChip } from './composer.js';
import { api, state, bootChat } from './app.js';

/* ------------------------------------------------------------------------ */
/* Overlays: Settings and first-run onboarding                               */
/* ------------------------------------------------------------------------ */

/** Where a provider's key comes from, as one plain phrase. Only the state that
 *  needs acting on is colored. */
const KEY_STATE = {
  stored: { text: 'key stored' },
  env: { text: 'key from env' },
  oauth: { text: 'signed in' },
  not_needed: { text: 'local' },
  missing: { text: 'no key', tone: 'warn' },
};

/** The host of a base URL — enough to tell providers apart, and short enough
 *  not to wrap (Cloudflare's is a path template with an account-id slot). */
function endpointHost(url) {
  const bare = String(url || '').replace(/^https?:\/\//, '').replace(/\/+$/, '');
  return bare.split('/', 1)[0];
}

function closeOverlay() {
  $('overlay-root').replaceChildren();
  document.removeEventListener('keydown', onOverlayKeydown);
}

function onOverlayKeydown(e) {
  if (e.key === 'Escape') closeOverlay();
}

/** Mount `panel` in a dimmed overlay. `dismissable` false = onboarding, which
 *  has nothing behind it worth clicking. */
function showOverlay(panel, { dismissable = true } = {}) {
  const overlay = h('div', {
    class: 'overlay',
    onclick: dismissable ? (e) => { if (e.target === overlay) closeOverlay(); } : null,
  }, panel);
  $('overlay-root').replaceChildren(overlay);
  if (dismissable) document.addEventListener('keydown', onOverlayKeydown);
  return overlay;
}

/** A labelled field: micro-label above the input. */
function field(label, input, hint) {
  return h('label', { class: 'field' },
    h('span', { class: 'field-label' }, label),
    input,
    hint && h('span', { class: 'field-hint' }, hint));
}

const textInput = (attrs) => h('input', { class: 'input mono', type: 'text', spellcheck: 'false', ...attrs });

/** The custom-provider pseudo-preset: any OpenAI-compatible endpoint. */
const CUSTOM_PRESET = {
  name: '', label: 'Custom', kind: 'openai', base_url: '', model: '',
  needs_key: true, custom: true,
};

/**
 * The provider form, shared by onboarding and Settings.
 * @param {Object} preset  a preset, or an existing provider to edit
 */
function providerForm(preset, { submitLabel = 'Save', onSaved, onCancel } = {}) {
  const editing = !!preset.editing;
  const local = preset.kind === 'ollama' || preset.kind === 'llamacpp';
  const needsKey = preset.needs_key !== false && !local;

  const nameInput = textInput({ value: preset.name || '', placeholder: 'name', readonly: editing || null });
  const baseInput = textInput({ value: preset.base_url || '', placeholder: 'https://…' });
  const modelInput = textInput({ value: preset.model || '', placeholder: 'model tag' });
  const keyInput = h('input', {
    class: 'input mono', type: 'password', spellcheck: 'false', autocomplete: 'off',
    placeholder: editing ? 'unchanged' : 'sk-…',
  });
  const note = h('div', { class: 'note error hidden' });
  const submit = h('button', { class: 'btn primary', type: 'submit' }, submitLabel);

  const form = h('form', { class: 'form' },
    ...[
      !editing && preset.custom && field('Name', nameInput),
      (preset.needs_base_url || preset.custom || editing) && field('Base URL', baseInput),
      field('Model', modelInput),
      needsKey && field('API key', keyInput, 'stored in ~/.wizard/credentials.toml'),
    ].filter(Boolean),
    note,
    h('div', { class: 'form-actions' },
      submit,
      onCancel && h('button', { class: 'btn quiet', type: 'button', onclick: onCancel }, 'Cancel')));

  form.onsubmit = async (e) => {
    e.preventDefault();
    note.className = 'note error hidden';
    submit.setAttribute('disabled', '');
    submit.textContent = 'Checking…';
    try {
      const { settings, probe } = await api.saveProvider({
        name: (nameInput.value || preset.name).trim(),
        kind: preset.kind,
        baseUrl: baseInput.value.trim() || preset.base_url,
        model: modelInput.value.trim(),
        apiKey: keyInput.value.trim() || undefined,
      });
      state.settings = settings;
      // Saved either way: a provider that does not answer is still worth
      // keeping on the page so its key or URL can be fixed.
      if (onSaved) onSaved({ settings, probe });
    } catch (err) {
      note.className = 'note error';
      note.textContent = String((err && err.message) || err);
    } finally {
      submit.removeAttribute('disabled');
      submit.textContent = submitLabel;
    }
  };
  return form;
}

/** The provider list both overlays choose from: a name and where it points. */
function presetList(presets, onPick) {
  const row = (p, endpoint) =>
    h('button', { class: 'row row-pick', type: 'button', onclick: () => onPick(p) },
      h('span', { class: 'row-name' }, p.label),
      h('span', { class: 'row-meta mono' }, endpoint));
  return h('div', { class: 'rows' },
    ...presets.map((p) => row(p, endpointHost(p.base_url))),
    row(CUSTOM_PRESET, 'OpenAI-compatible'));
}

/* --- Subscription sign-in (OAuth) ----------------------------------------- */

/** The plans you can sign in to, rather than paste a key for. */
export const SIGN_INS = [
  { id: 'chatgpt', label: 'ChatGPT', plan: 'Plus / Pro / Team subscription' },
  { id: 'xai', label: 'xAI', plan: 'SuperGrok subscription' },
];

/**
 * Sign in to a subscription: the browser goes to the provider, which redirects
 * back to the loopback listener that flow bound for itself (never a route this
 * server serves — a provider only redirects to the address registered with its
 * client id), and we poll /api/login for the outcome.
 *
 * The popup is opened synchronously from the click — a browser blocks a window
 * opened after an await — and pointed at the authorize URL once we have it.
 */
function signInRow(id, label, plan, { onDone, onStatus }) {
  const say = onStatus || (() => {});
  return h('button', {
    class: 'row row-pick row-signin', type: 'button',
    dataset: { provider: id }, // `/login <plan>` focuses the row it names
    onclick: async () => {
      const tab = window.open('', '_blank');
      try {
        const url = await api.beginSignIn(id);
        if (tab) tab.location = url;
        else window.location = url; // popups blocked: use this tab
        say(`Waiting for ${label} in the other tab…`);
        await waitForSignIn();
        say(null);
        if (onDone) onDone();
      } catch (err) {
        if (tab) tab.close();
        say(String((err && err.message) || err), true);
      }
    },
  },
    h('span', { class: 'row-name' }, `Sign in with ${label}`),
    h('span', { class: 'row-meta' }, plan));
}

/** Poll until the sign-in in flight finishes, one way or the other. */
async function waitForSignIn({ timeoutMs = 5 * 60 * 1000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    await new Promise((r) => setTimeout(r, 1000));
    const status = await api.signInStatus();
    if (status.state === 'done') return status;
    if (status.state === 'failed') throw new Error(status.error || 'sign-in failed');
    // `idle` means the flow was dropped — the server restarted under us.
    if (status.state === 'idle') throw new Error('the sign-in was not completed');
    if (Date.now() > deadline) throw new Error('the sign-in timed out');
  }
}

/* --- Onboarding ----------------------------------------------------------- */

/** First run: no provider is configured, so wizard cannot answer anything yet. */
export function openOnboarding(settings) {
  const body = h('div', { class: 'sheet-body' });
  const sheet = h('div', { class: 'sheet onboard', role: 'dialog', 'aria-label': 'Set up wizard' },
    h('div', { class: 'sheet-head' },
      h('h2', { class: 'sheet-title' }, 'Set up wizard'),
      h('p', { class: 'sheet-sub' }, 'Pick a provider to run the agent on.')),
    body);

  const skip = (label) => h('button', {
    class: 'btn quiet', type: 'button',
    title: 'Wizard cannot answer until a provider is configured',
    onclick: () => { closeOverlay(); bootChat(); },
  }, label);

  const pickStep = () => {
    const note = h('div', { class: 'note hidden' });
    const say = (text, bad) => {
      note.textContent = text || '';
      note.className = `note${bad ? ' error' : ''}${text ? '' : ' hidden'}`;
    };
    const signedIn = async () => {
      state.settings = await api.settings();
      closeOverlay();
      bootChat();
    };
    fillWith(body,
      // A subscription first: it is what most people already have, and it needs
      // no key to paste.
      h('div', { class: 'rows' },
        ...SIGN_INS.map((s) => signInRow(s.id, s.label, s.plan, { onDone: signedIn, onStatus: say }))),
      note,
      h('div', { class: 'block-title with-rule' }, 'or use an API key'),
      presetList(settings.presets, formStep),
      h('div', { class: 'form-actions end' }, skip('Skip')),
    );
  };

  const formStep = (preset) => {
    const done = ({ probe }) => {
      if (!probe.ok) {
        fillWith(body,
          h('div', { class: 'note error' },
            `Saved, but ${preset.label} did not answer: ${probe.error || 'unknown error'}`),
          h('div', { class: 'form-actions' },
            h('button', { class: 'btn primary', type: 'button', onclick: () => formStep(preset) }, 'Try again'),
            skip('Continue anyway')),
        );
        return;
      }
      closeOverlay();
      bootChat();
    };
    fillWith(body,
      h('div', { class: 'block-title' }, preset.label),
      providerForm(preset, { submitLabel: 'Connect', onSaved: done, onCancel: pickStep }),
    );
  };

  pickStep();
  showOverlay(sheet, { dismissable: false });
}

/* --- Settings ------------------------------------------------------------- */

/**
 * Settings. `focus` is what `/provider` and `/login` come in on: this is the one
 * sheet either of them names, so they open it on the part they mean rather than
 * on a page of their own.
 * @param {{focus?: 'providers'|'signin'|null, signIn?: string|null}} [opts]
 *   `signIn` names the plan `/login <xai>` asked for, whose row is focused —
 *   focused, not clicked: the consent window has to open on the user's own press
 *   or the browser blocks it.
 */
export async function openSettings({ focus = null, signIn = null } = {}) {
  const body = h('div', { class: 'sheet-body' });
  const foot = h('div', { class: 'sheet-foot mono' });
  const sheet = h('div', { class: 'sheet settings', role: 'dialog', 'aria-label': 'Settings' },
    h('div', { class: 'sheet-head row-between' },
      h('h2', { class: 'sheet-title' }, 'Settings'),
      iconBtn('close', 'Close', closeOverlay)),
    body, foot);
  showOverlay(sheet);
  body.append(h('div', { class: 'note' }, 'Loading…'));
  try {
    state.settings = await api.settings();
  } catch (err) {
    body.replaceChildren(h('div', { class: 'note error' }, String((err && err.message) || err)));
    return;
  }
  renderSettings(body, foot, { focus, signIn });
}

function renderSettings(body, foot, { focus = null, signIn = null } = {}) {
  const s = state.settings;
  const rerender = () => renderSettings(body, foot);

  const providerRow = (p) => {
    const key = KEY_STATE[p.key] || KEY_STATE.missing;
    const status = h('div', { class: 'row-status hidden' });
    const say = (text, bad) => {
      status.textContent = text;
      status.classList.remove('hidden');
      status.classList.toggle('error', !!bad);
    };
    const act = async (fn) => {
      try {
        const out = await fn();
        if (out && out.providers) state.settings = out;
        rerender();
      } catch (err) {
        say(String((err && err.message) || err), true);
      }
    };
    const test = async () => {
      say('Testing…');
      try {
        const probe = await api.testProvider(p.name);
        say(probe.ok ? `Answered — ${probe.models.length || 'no'} models` : probe.error || 'no answer', !probe.ok);
      } catch (err) {
        say(String((err && err.message) || err), true);
      }
    };
    const edit = () => {
      fillWith(body,
        h('div', { class: 'block' },
          h('div', { class: 'block-title' }, p.name),
          providerForm({ ...p, editing: true, needs_key: p.key !== 'not_needed' }, {
            onSaved: rerender, onCancel: rerender,
          })));
    };
    const action = (label, onclick, cls = '') =>
      h('button', { class: `link ${cls}`.trim(), type: 'button', onclick }, label);

    return h('div', { class: 'row row-provider' + (p.active ? ' is-active' : '') },
      h('div', { class: 'row-main' },
        h('div', { class: 'row-line' },
          h('span', { class: 'row-name' }, p.name),
          p.active && h('span', { class: 'tag' }, 'active')),
        h('div', { class: 'row-meta mono' },
          `${p.kind} · ${p.model} · `,
          h('span', { class: key.tone || '' }, key.text)),
        status),
      h('div', { class: 'row-actions' },
        !p.active && action('Use', () => act(() => api.activateProvider(p.name))),
        action('Test', test),
        action('Edit', edit),
        action('Remove', () => act(() => api.removeProvider(p.name)), 'danger')));
  };

  // The picker lives inside the Providers block: it is the same list, one
  // step further in, not a second section competing with it.
  const add = h('div', { class: 'add-provider' });
  const resetAdd = () => {
    fillWith(add, h('button', {
      class: 'row row-add', type: 'button',
      onclick: () => showChoices(),
    }, h('span', { class: 'row-name' }, '+  Add provider')));
  };
  const showChoices = () => {
    const note = h('div', { class: 'note hidden' });
    const say = (text, bad) => {
      note.textContent = text || '';
      note.className = `note${bad ? ' error' : ''}${text ? '' : ' hidden'}`;
    };
    fillWith(add,
      h('div', { class: 'rows' },
        ...SIGN_INS.map((si) => signInRow(si.id, si.label, si.plan, { onDone: rerender, onStatus: say }))),
      note,
      h('div', { class: 'block-title with-rule' }, 'or use an API key'),
      presetList(s.presets, pickPreset),
      h('div', { class: 'form-actions end' },
        h('button', { class: 'btn quiet', type: 'button', onclick: resetAdd }, 'Cancel')),
    );
  };
  const pickPreset = (preset) => {
    fillWith(add,
      h('div', { class: 'block-title' }, `Add ${preset.label}`),
      providerForm(preset, { onSaved: rerender, onCancel: resetAdd }));
  };
  resetAdd();

  // Persists when the field is left, or on Enter. A number with a Save button
  // beside it is one control more than the job needs.
  const steps = h('input', {
    class: 'input num', type: 'number', min: '0', max: '1000', value: String(s.max_steps),
  });
  const agentNote = h('span', { class: 'note inline hidden' });
  steps.addEventListener('keydown', (e) => { if (e.key === 'Enter') steps.blur(); });
  steps.addEventListener('change', async () => {
    const raw = steps.value.trim();
    const value = Number(raw);
    // 0 is a value — it is how the limit is turned off — but an empty box is
    // not: clearing the field must not be read as a request for no limit.
    if (!raw || !Number.isInteger(value) || value < 0 || value === s.max_steps) {
      steps.value = String(s.max_steps);
      return;
    }
    try {
      state.settings = await api.saveSettings({ max_steps: value });
      s.max_steps = state.settings.max_steps;
      agentNote.className = 'note inline';
      agentNote.textContent = s.max_steps === 0 ? 'Saved — no limit' : 'Saved';
    } catch (err) {
      steps.value = String(s.max_steps);
      agentNote.className = 'note inline error';
      agentNote.textContent = String((err && err.message) || err);
    }
  });

  fillWith(body,
    h('div', { class: 'block' },
      h('div', { class: 'block-title' }, 'Providers'),
      s.providers.length
        ? h('div', { class: 'rows' }, ...s.providers.map(providerRow))
        : h('div', { class: 'note' }, 'None configured — wizard cannot answer until one is.'),
      add),
    h('div', { class: 'block' },
      h('div', { class: 'block-title' }, 'Agent'),
      h('div', { class: 'setting' },
        h('div', { class: 'setting-main' },
          h('div', { class: 'setting-name' }, 'Step limit'),
          h('div', { class: 'setting-help' }, 'Tool calls one chat may make per turn. 0 is no limit.')),
        h('div', { class: 'setting-control' }, agentNote, steps))),
  );
  foot.textContent = s.config_path;

  // `/provider` and `/login` open this sheet on the part of it they name: the
  // same picker the "Add provider" row opens, since a command that opened one of
  // its own would be a second way to do this that could drift from the first.
  if (focus) {
    showChoices();
    if (focus !== 'signin') {
      add.scrollIntoView({ block: 'nearest' });
    } else {
      const row = add.querySelector(signIn ? `.row-signin[data-provider="${signIn}"]` : '.row-signin');
      if (row) {
        row.scrollIntoView({ block: 'nearest' });
        row.focus(); // the press that opens the consent window has to be the user's own
      }
    }
  }

  // The composer's model chip reflects whatever the active provider is now.
  api.listModels().then((models) => {
    state.models = models;
    if (state.modelId && !models.some((m) => m.value === state.modelId)) {
      state.modelId = null;
      state.modelLabel = null;
    }
    updateModelChip();
  }).catch(() => { /* the chip keeps its last label */ });
}
