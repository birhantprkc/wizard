//! Settings → Plugins: browse the Pi package gallery and install a package
//! for Pi, for Wizard, or both, on the selected device.
//!
//! Pi packages bundle extensions, skills, prompt templates, and themes. The
//! gallery is every npm package tagged `pi-package` (what pi.dev/packages
//! lists), searched straight from the npm registry by this viewport.
//!
//! Pi keeps its packages itself, so its half goes through the engine's
//! relay-forwardable `*PiPackage*` RPCs (`pi install`). Wizard can use the
//! same packages in beta: skills and prompt templates work, extensions and
//! themes don't yet. Its half goes through the `*WizardPlugin*` RPCs, which
//! run `wizard plugins … --json` on the device, and an Install first asks
//! Wizard what would work (`inspect`) so the choice is made knowing. The
//! page-header device switcher (the Agents pattern) retargets all of it; a
//! device without Wizard, or with a Wizard too old for plugins, gets the
//! Pi-only page it always had.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};
use serde::Deserialize;
use zeron_engine::pi_packages::{PiPackage, PiPackageKind, PiPackages, package_key};
use zeron_engine::wizard_plugins::{WizardPlugin, WizardPluginChange, WizardPlugins};
use zeron_proto::HarnessId;
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const GALLERY_SEARCH_URL: &str = "https://registry.npmjs.org/-/v1/search";
const GALLERY_PAGE_URL: &str = "https://pi.dev/packages";
const GALLERY_SIZE: &str = "40";
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
const BETA_LINE: &str =
    "Wizard can use Pi plugins in beta: skills and prompts work, extensions don't yet.";

/// One gallery result (an npm search hit tagged `pi-package`).
#[derive(Clone, Debug, PartialEq)]
struct GalleryPackage {
    name: String,
    version: String,
    description: Option<String>,
    publisher: Option<String>,
    weekly_downloads: Option<u64>,
    url: String,
}

/// Which harness an install goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallTarget {
    Both,
    Wizard,
    Pi,
}

impl InstallTarget {
    fn label(self) -> &'static str {
        match self {
            InstallTarget::Both => "Pi and Wizard",
            InstallTarget::Wizard => "Only Wizard",
            InstallTarget::Pi => "Only Pi",
        }
    }

    fn wizard(self) -> bool {
        matches!(self, InstallTarget::Both | InstallTarget::Wizard)
    }

    fn pi(self) -> bool {
        matches!(self, InstallTarget::Both | InstallTarget::Pi)
    }
}

/// The choices the "Install for" row offers, given which agents can take a
/// package on this device.
pub(crate) fn target_options(pi: bool, wizard: bool) -> Vec<InstallTarget> {
    let mut options = Vec::new();
    if pi && wizard {
        options.push(InstallTarget::Both);
    }
    if wizard {
        options.push(InstallTarget::Wizard);
    }
    if pi {
        options.push(InstallTarget::Pi);
    }
    options
}

/// The preselected choice: both when both agents are here and neither has
/// the package, otherwise whichever is still missing it. `None` when no
/// agent can take it.
pub(crate) fn default_target(
    pi: bool,
    wizard: bool,
    in_pi: bool,
    in_wizard: bool,
) -> Option<InstallTarget> {
    match (pi, wizard) {
        (true, true) => Some(match (in_pi, in_wizard) {
            (true, false) => InstallTarget::Wizard,
            (false, true) => InstallTarget::Pi,
            _ => InstallTarget::Both,
        }),
        (false, true) => Some(InstallTarget::Wizard),
        (true, false) => Some(InstallTarget::Pi),
        (false, false) => None,
    }
}

/// One line on what Wizard makes of a package.
pub(crate) fn support_line(plugin: &WizardPlugin) -> String {
    let (works, missing) = plugin.support();
    match (works.is_empty(), missing.is_empty()) {
        (true, true) => "Wizard found no skills, prompts, or extensions in it.".to_string(),
        (true, false) => format!("Nothing in it runs in Wizard yet ({missing})."),
        (false, true) => format!("In Wizard: {works}."),
        (false, false) => format!("In Wizard: {works}. Not supported yet: {missing}."),
    }
}

/// A package installed for Pi, Wizard, or both, keyed the way Pi tells
/// packages apart.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InstalledRow {
    pub key: String,
    pub pi: Option<PiPackage>,
    pub wizard: Option<WizardPlugin>,
}

impl InstalledRow {
    fn name(&self) -> String {
        self.wizard
            .as_ref()
            .map(|w| w.name.clone())
            .or_else(|| self.pi.as_ref().map(|p| p.name.clone()))
            .unwrap_or_else(|| self.key.clone())
    }
}

pub(crate) fn merge_installed(pi: &[PiPackage], wizard: &[WizardPlugin]) -> Vec<InstalledRow> {
    let mut rows: BTreeMap<String, InstalledRow> = BTreeMap::new();
    for package in pi {
        rows.entry(package.name.clone())
            .or_insert_with(|| InstalledRow {
                key: package.name.clone(),
                pi: None,
                wizard: None,
            })
            .pi = Some(package.clone());
    }
    for plugin in wizard {
        let key = package_key(&plugin.source);
        rows.entry(key.clone())
            .or_insert_with(|| InstalledRow {
                key,
                pi: None,
                wizard: None,
            })
            .wizard = Some(plugin.clone());
    }
    rows.into_values().collect()
}

/// The "Install for" chooser, open under the row (or source field) it
/// belongs to.
#[derive(Clone, Debug)]
struct Choice {
    source: String,
    target: InstallTarget,
    /// Opened from the typed source field rather than a gallery row.
    typed: bool,
    /// What Wizard would install, when Wizard is one of the options.
    inspect: Loadable<WizardPlugin>,
}

/// The one package operation in flight — Pi serializes them anyway, and one
/// at a time keeps each row's spinner honest.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    InstallPi,
    Install {
        source: String,
        target: InstallTarget,
    },
    RemovePi(String),
    RemoveWizard(String),
    Update(Option<String>),
}

pub struct PluginsPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    /// Which device's agents are shown/edited; `None` = this device.
    target_device: Option<String>,
    device_menu_open: bool,
    device_menu_pressed_open: bool,
    packages: Loadable<PiPackages>,
    wizard: Loadable<WizardPlugins>,
    load_task: Option<Task<()>>,
    wizard_task: Option<Task<()>>,
    pending: Option<Pending>,
    op_task: Option<Task<()>>,
    error: Option<String>,
    notice: Option<String>,
    choice: Option<Choice>,
    inspect_task: Option<Task<()>>,
    search: Entity<ComposerInput>,
    source: Entity<ComposerInput>,
    gallery: Loadable<Vec<GalleryPackage>>,
    gallery_query: Option<String>,
    gallery_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PluginsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            ComposerInput::new("Search the Pi gallery", cx)
                .with_single_line()
                .with_accessibility_role(gpui::Role::SearchInput)
                .with_text_metrics(13.0, 18.0)
        });
        let source = cx.new(|cx| {
            ComposerInput::new(
                "npm:@scope/package, git:github.com/owner/repo, or a URL",
                cx,
            )
            .with_single_line()
            .with_text_metrics(13.0, 18.0)
        });
        let subscriptions = vec![
            cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
                ComposerInputEvent::Edited => this.search_gallery(false, cx),
                ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted => {
                    this.search_gallery(true, cx)
                }
                _ => {}
            }),
            cx.subscribe(&source, |this: &mut Self, _, event, cx| {
                if matches!(
                    event,
                    ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted
                ) {
                    this.install_from_source(cx);
                }
            }),
        ];
        let mut page = Self {
            state,
            scroll: widgets::PageScroll::default(),
            target_device: None,
            device_menu_open: false,
            device_menu_pressed_open: false,
            packages: Loadable::Idle,
            wizard: Loadable::Idle,
            load_task: None,
            wizard_task: None,
            pending: None,
            op_task: None,
            error: None,
            notice: None,
            choice: None,
            inspect_task: None,
            search,
            source,
            gallery: Loadable::Idle,
            gallery_query: None,
            gallery_task: None,
            _subscriptions: subscriptions,
        };
        page.load(cx);
        page.search_gallery(true, cx);
        page
    }

    /// Params with the `targetDeviceId` passthrough merged in.
    fn with_target(&self, mut value: serde_json::Value) -> serde_json::Value {
        if let (Some(target), Some(object)) = (&self.target_device, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.device_menu_open = false;
        if self.target_device == target {
            cx.notify();
            return;
        }
        self.target_device = target;
        self.pending = None;
        self.op_task = None;
        self.error = None;
        self.notice = None;
        self.choice = None;
        self.inspect_task = None;
        self.packages = Loadable::Idle;
        self.wizard = Loadable::Idle;
        self.load(cx);
        cx.notify();
    }

    /// Pi can take a package on the target device.
    fn pi_ready(&self) -> bool {
        self.packages.ready().is_some_and(|p| p.pi_installed)
    }

    /// Wizard can take a package on the target device. An engine too old to
    /// know the Wizard RPCs answers with an error, which reads as "no".
    fn wizard_ready(&self) -> bool {
        self.wizard.ready().is_some_and(WizardPlugins::available)
    }

    fn wizard_plugins(&self) -> &[WizardPlugin] {
        self.wizard
            .ready()
            .map(|w| w.plugins.as_slice())
            .unwrap_or_default()
    }

    fn pi_packages(&self) -> &[PiPackage] {
        self.packages
            .ready()
            .filter(|p| p.pi_installed)
            .map(|p| p.packages.as_slice())
            .unwrap_or_default()
    }

    /// Both lists, in parallel. Each reply is dropped if the device switcher
    /// moved on while it was in flight.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({}));
        if !matches!(self.packages, Loadable::Ready(_)) {
            self.packages = Loadable::Loading;
        }
        if !matches!(self.wizard, Loadable::Ready(_)) {
            self.wizard = Loadable::Loading;
        }
        let target = self.target_device.clone();
        let pi_engine = engine.clone();
        let pi_params = params.clone();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = pi_engine
                .client()
                .call(methods::LIST_PI_PACKAGES, pi_params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<PiPackages>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.packages = match result {
                    Ok(list) => Loadable::Ready(list),
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
        let target = self.target_device.clone();
        self.wizard_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_WIZARD_PLUGINS, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<WizardPlugins>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.wizard = match result {
                    Ok(list) => Loadable::Ready(list),
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Start an install: open the "Install for" chooser when Wizard is an
    /// option (it shows what would work there), or go straight to Pi when
    /// Pi is the only agent that can take it.
    fn begin_install(&mut self, source: String, typed: bool, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let pi = self.pi_ready();
        let wizard = self.wizard_ready();
        if !wizard {
            if pi {
                self.run(
                    Pending::Install {
                        source,
                        target: InstallTarget::Pi,
                    },
                    cx,
                );
            }
            return;
        }
        let key = package_key(&source);
        let in_pi = self.pi_packages().iter().any(|p| p.name == key);
        let in_wizard = self
            .wizard_plugins()
            .iter()
            .any(|p| package_key(&p.source) == key);
        let target = default_target(pi, wizard, in_pi, in_wizard).unwrap_or(InstallTarget::Wizard);
        self.error = None;
        self.notice = None;
        self.choice = Some(Choice {
            source: source.clone(),
            target,
            typed,
            inspect: Loadable::Loading,
        });
        self.inspect(source, cx);
        cx.notify();
    }

    fn inspect(&mut self, source: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({ "source": source }));
        self.inspect_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::INSPECT_WIZARD_PLUGIN, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<WizardPlugin>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if let Some(choice) = page.choice.as_mut()
                    && choice.source == source
                {
                    choice.inspect = match result {
                        Ok(plugin) => {
                            // Nothing Wizard can run: Pi is the only
                            // sensible target, when Pi is here.
                            if plugin.support().0.is_empty() && choice.target == InstallTarget::Both
                            {
                                choice.target = InstallTarget::Pi;
                            }
                            Loadable::Ready(plugin)
                        }
                        Err(error) => Loadable::Error(error),
                    };
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn confirm_choice(&mut self, cx: &mut Context<Self>) {
        let Some(choice) = self.choice.take() else {
            return;
        };
        self.inspect_task = None;
        self.run(
            Pending::Install {
                source: choice.source,
                target: choice.target,
            },
            cx,
        );
    }

    fn cancel_choice(&mut self, cx: &mut Context<Self>) {
        self.choice = None;
        self.inspect_task = None;
        cx.notify();
    }

    /// Run one package operation on the target device: its RPCs in order,
    /// stopping at the first failure, then both lists refreshed.
    fn run(&mut self, pending: Pending, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let calls: Vec<(&'static str, serde_json::Value)> = match &pending {
            Pending::InstallPi => vec![(
                methods::INSTALL_HARNESS,
                serde_json::json!({ "harness": HarnessId::Pi }),
            )],
            Pending::Install { source, target } => {
                let mut calls = Vec::new();
                if target.wizard() {
                    calls.push((
                        methods::INSTALL_WIZARD_PLUGIN,
                        serde_json::json!({ "source": source }),
                    ));
                }
                if target.pi() {
                    calls.push((
                        methods::INSTALL_PI_PACKAGE,
                        serde_json::json!({ "source": source }),
                    ));
                }
                calls
            }
            Pending::RemovePi(source) => vec![(
                methods::REMOVE_PI_PACKAGE,
                serde_json::json!({ "source": source }),
            )],
            Pending::RemoveWizard(name) => vec![(
                methods::REMOVE_WIZARD_PLUGIN,
                serde_json::json!({ "name": name }),
            )],
            Pending::Update(source) => vec![(
                methods::UPDATE_PI_PACKAGES,
                match source {
                    Some(source) => serde_json::json!({ "source": source }),
                    None => serde_json::json!({}),
                },
            )],
        };
        let calls: Vec<_> = calls
            .into_iter()
            .map(|(method, params)| (method, self.with_target(params)))
            .collect();
        let target = self.target_device.clone();
        self.pending = Some(pending.clone());
        self.error = None;
        self.notice = None;
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let mut done: Vec<&'static str> = Vec::new();
            let mut installed: Option<WizardPlugin> = None;
            let mut failure: Option<String> = None;
            for (method, params) in calls {
                match engine.client().call(method, params).await {
                    Ok(value) => {
                        if method == methods::INSTALL_WIZARD_PLUGIN {
                            installed = serde_json::from_value::<WizardPluginChange>(value)
                                .ok()
                                .map(|change| change.plugin);
                        }
                        done.push(method);
                    }
                    Err(error) => {
                        failure = Some(error.to_string());
                        break;
                    }
                }
            }
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.pending = None;
                match failure {
                    None => {
                        if pending == Pending::InstallPi {
                            // InstallHarness replies with the harness catalog;
                            // let the composer see the new agent.
                            crate::pickers::bump_harness_catalog(cx);
                        }
                        if matches!(pending, Pending::Install { .. }) {
                            page.source.update(cx, |input, cx| input.set_text("", cx));
                        }
                        page.notice = Some(success_notice(&pending, installed.as_ref()));
                    }
                    Some(error) => page.error = Some(failure_notice(&pending, &done, &error)),
                }
                page.load(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn install_from_source(&mut self, cx: &mut Context<Self>) {
        let typed = self.source.read(cx).text().trim().to_string();
        if typed.is_empty() {
            return;
        }
        match zeron_engine::pi_packages::normalize_source(&typed) {
            Ok(source) => self.begin_install(source, true, cx),
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    /// Search the gallery for the current query (debounced while typing).
    /// Replacing the task aborts a superseded request.
    fn search_gallery(&mut self, immediate: bool, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().trim().to_string();
        if self.gallery_query.as_deref() == Some(query.as_str())
            && matches!(self.gallery, Loadable::Ready(_) | Loadable::Loading)
            && !immediate
        {
            return;
        }
        self.gallery_query = Some(query.clone());
        if !matches!(self.gallery, Loadable::Ready(_)) {
            self.gallery = Loadable::Loading;
        }
        let request = gpui_tokio::Tokio::spawn(cx, async move {
            if !immediate {
                tokio::time::sleep(SEARCH_DEBOUNCE).await;
            }
            fetch_gallery(&query).await
        });
        self.gallery_task = Some(cx.spawn(async move |this, cx| {
            let result = match request.await {
                Ok(result) => result,
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |page, cx| {
                page.gallery = match result {
                    Ok(list) => Loadable::Ready(list),
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    /// The page-header device switcher (the Agents/Accounts pattern).
    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (mut devices, local_id) = {
            let s = self.state.read(cx);
            (s.devices.clone(), s.local_device_id.clone())
        };
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|d| Some(d.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|d| d.platform.as_str())
                .unwrap_or("macos"),
        );
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|d| d.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let emerald = theme.success;
        let open = self.device_menu_open;

        let mut trigger =
            div()
                .id("plugins-device-switcher")
                .flex_none()
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .bg(if open {
                    crate::theme::ink(0.06)
                } else {
                    gpui::transparent_black()
                })
                .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.04))))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.device_menu_pressed_open = this.device_menu_open;
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let pressed_open = std::mem::take(&mut this.device_menu_pressed_open);
                    this.device_menu_open = !pressed_open && !this.device_menu_open;
                    cx.notify();
                }))
                .child(
                    icon(trigger_glyph)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(trigger_label),
                )
                .child(div().size(px(6.0)).rounded_full().flex_none().bg(
                    if effective == local_id {
                        emerald
                    } else {
                        crate::theme::ink(0.2)
                    },
                ))
                .child(
                    icon(icons::SORT_VERTICAL)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.4 })),
                );

        if open {
            let theme = &theme.for_popup();
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(ix, d)| {
                    let is_active = Some(d.id.as_str()) == effective.as_deref();
                    let is_local = local_id.as_deref() == Some(d.id.as_str());
                    let glyph = platform_glyph(&d.platform);
                    let name: SharedString = d.name.clone().into();
                    let pick_id = d.id.clone();
                    popover::menu_row(theme, is_active, format!("plugins-device-row-{ix}"))
                        .id(("plugins-device-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let target = (!is_local).then(|| pick_id.clone());
                            this.set_target_device(target, cx);
                        }))
                        .child(
                            icon(glyph)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(is_local, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from("You")),
                            )
                        })
                        .child(
                            div()
                                .size(px(6.0))
                                .rounded_full()
                                .flex_none()
                                .bg(if is_local {
                                    emerald
                                } else {
                                    crate::theme::ink(0.2)
                                }),
                        )
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("plugins-device-menu", menu, None));
        }
        trigger.into_any_element()
    }

    fn spinner_line(
        &self,
        key: String,
        label: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(crate::loaders::mini_mono_spinner(
                key,
                1.5,
                theme.text_muted,
                cx.entity_id(),
                cx,
            ))
            .child(SharedString::from(label.to_string()))
            .into_any_element()
    }

    /// A ghost action that goes inert while another operation runs.
    fn action(
        &self,
        theme: &Theme,
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let idle = self.pending.is_none();
        widgets::ghost_action(theme)
            .id(id)
            .flex_none()
            .when(!idle, |el| el.opacity(0.4).cursor_default())
            .when(idle, |el| {
                el.hover(|s| widgets::ghost_hover(theme, s))
                    .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            })
            .child(SharedString::from(label))
            .into_any_element()
    }

    /// The "Beta" pill and the one line about what Wizard runs.
    fn beta_line(&self, theme: &Theme) -> AnyElement {
        div()
            .mt(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .child(widgets::badge(theme, "Beta"))
            .child(div().min_w_0().child(SharedString::from(BETA_LINE)))
            .into_any_element()
    }

    fn render_install_pi(
        &self,
        packages: &PiPackages,
        wizard: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let installing = self.pending == Some(Pending::InstallPi);
        let mut meta = vec![
            div()
                .child(SharedString::from(if wizard {
                    "Wizard can take plugins already. Install Pi to add them there too."
                } else {
                    "Extensions run inside the Pi coding agent. Install Pi on this device to add them."
                }))
                .into_any_element(),
        ];
        if installing {
            meta.push(self.spinner_line("pi-install-spinner".into(), "Installing Pi…", theme, cx));
        } else if !packages.can_install_pi {
            meta.push(
                div()
                    .text_color(theme.warning_muted.opacity(0.9))
                    .child(SharedString::from(format!(
                        "npm isn't available here. Install Node.js, or run: {}",
                        zeron_harness::install::manual_command(HarnessId::Pi).unwrap_or_default()
                    )))
                    .into_any_element(),
            );
        }
        let (mark, _) = crate::pickers::harness_brand_icon(HarnessId::Pi);
        widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, mark))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(
                                theme,
                                "Pi isn't installed on this device",
                            ))
                            .child(widgets::meta_line(theme, meta)),
                    )
                    .when(packages.can_install_pi && !installing, |el| {
                        el.child(self.action(
                            theme,
                            "pi-install",
                            "Install Pi",
                            |this, cx| this.run(Pending::InstallPi, cx),
                            cx,
                        ))
                    }),
            )
            .into_any_element()
    }

    fn render_installed(
        &self,
        rows: &[InstalledRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let updating_all = self.pending == Some(Pending::Update(None));
        let any_pi = rows.iter().any(|row| row.pi.is_some());
        let header = div()
            .mt(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(widgets::field_label(theme, "Installed"))
            .when(any_pi, |el| {
                el.child(if updating_all {
                    self.spinner_line("pi-update-all-spinner".into(), "Updating…", theme, cx)
                } else {
                    self.action(
                        theme,
                        "pi-update-all",
                        "Update all",
                        |this, cx| this.run(Pending::Update(None), cx),
                        cx,
                    )
                })
            });
        let card = if rows.is_empty() {
            widgets::section_card(theme).mt(px(10.0)).child(
                widgets::card_row(theme, true).child(
                    div()
                        .text_size(crate::typography::ui_rems(widgets::ROW_DESCRIPTION_SIZE))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child(SharedString::from(
                            "No plugins yet. Pick one from the gallery below.",
                        )),
                ),
            )
        } else {
            widgets::section_card(theme).mt(px(10.0)).children(
                rows.iter()
                    .enumerate()
                    .map(|(ix, row)| self.installed_row(ix, row, theme, cx)),
            )
        };
        div().child(header).child(card).into_any_element()
    }

    fn installed_row(
        &self,
        ix: usize,
        row: &InstalledRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pi_source = row.pi.as_ref().map(|p| p.source.clone());
        let wizard_name = row.wizard.as_ref().map(|w| w.name.clone());
        let removing_pi = pi_source
            .as_ref()
            .is_some_and(|s| self.pending == Some(Pending::RemovePi(s.clone())));
        let removing_wizard = wizard_name
            .as_ref()
            .is_some_and(|n| self.pending == Some(Pending::RemoveWizard(n.clone())));
        let updating = pi_source
            .as_ref()
            .is_some_and(|s| self.pending == Some(Pending::Update(Some(s.clone()))));
        let mut meta: Vec<AnyElement> = Vec::new();
        let version = row
            .pi
            .as_ref()
            .and_then(|p| p.version.clone())
            .or_else(|| row.wizard.as_ref().and_then(|w| w.version.clone()));
        if let Some(version) = version {
            meta.push(
                div()
                    .child(SharedString::from(format!("v{version}")))
                    .into_any_element(),
            );
        }
        if let Some(package) = &row.pi {
            meta.push(
                div()
                    .child(SharedString::from(match package.kind {
                        PiPackageKind::Npm => "npm",
                        PiPackageKind::Git => "git",
                        PiPackageKind::Local => "local",
                    }))
                    .into_any_element(),
            );
            if package.filtered {
                meta.push(
                    div()
                        .child(SharedString::from("filtered"))
                        .into_any_element(),
                );
            }
        }
        if let Some(plugin) = &row.wizard {
            meta.push(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(support_line(plugin)))
                    .into_any_element(),
            );
        } else if let Some(description) = row.pi.as_ref().and_then(|p| p.description.clone()) {
            meta.push(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(description))
                    .into_any_element(),
            );
        }
        if removing_pi || removing_wizard {
            meta.push(self.spinner_line(
                format!("plugin-remove-spinner-{ix}"),
                "Removing…",
                theme,
                cx,
            ));
        }
        if updating {
            meta.push(self.spinner_line(
                format!("plugin-update-spinner-{ix}"),
                "Updating…",
                theme,
                cx,
            ));
        }
        let busy = removing_pi || removing_wizard || updating;
        let both = row.pi.is_some() && row.wizard.is_some();
        let can_update = row
            .pi
            .as_ref()
            .is_some_and(|p| p.kind != PiPackageKind::Local);
        widgets::card_row(theme, ix == 0)
            .id(("plugin-installed-row", ix))
            .child(widgets::row_tile(theme, icons::WIDGET))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .min_w_0()
                            .child(widgets::row_title(theme, row.name()))
                            .when(row.pi.is_some(), |el| el.child(widgets::badge(theme, "Pi")))
                            .when(row.wizard.is_some(), |el| {
                                el.child(widgets::badge(theme, "Wizard · beta"))
                            }),
                    )
                    .child(widgets::meta_line(theme, meta)),
            )
            .when(!busy && can_update, |el| {
                let source = pi_source.clone().unwrap_or_default();
                el.child(self.action(
                    theme,
                    ("plugin-update", ix),
                    "Update",
                    move |this, cx| this.run(Pending::Update(Some(source.clone())), cx),
                    cx,
                ))
            })
            .when_some(pi_source.clone().filter(|_| !busy), |el, source| {
                el.child(self.action(
                    theme,
                    ("plugin-remove-pi", ix),
                    if both { "Remove from Pi" } else { "Remove" },
                    move |this, cx| this.run(Pending::RemovePi(source.clone()), cx),
                    cx,
                ))
            })
            .when_some(wizard_name.clone().filter(|_| !busy), |el, name| {
                el.child(self.action(
                    theme,
                    ("plugin-remove-wizard", ix),
                    if both { "Remove from Wizard" } else { "Remove" },
                    move |this, cx| this.run(Pending::RemoveWizard(name.clone()), cx),
                    cx,
                ))
            })
            .into_any_element()
    }

    /// The "Install for" chooser: targets, the beta line, what Wizard makes
    /// of the package, and Install / Cancel.
    fn render_choice(&self, choice: &Choice, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let options = target_options(self.pi_ready(), self.wizard_ready());
        let pills = options
            .into_iter()
            .enumerate()
            .map(|(ix, option)| {
                let selected = option == choice.target;
                div()
                    .id(("plugin-target", ix))
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded_full()
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border })
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(if selected {
                        theme.accent
                    } else {
                        theme.text_muted
                    })
                    .cursor_pointer()
                    .role(gpui::Role::Button)
                    .aria_label(option.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(choice) = this.choice.as_mut() {
                            choice.target = option;
                            cx.notify();
                        }
                    }))
                    .child(SharedString::from(option.label()))
            })
            .collect::<Vec<_>>();
        let wizard_note: Option<AnyElement> =
            choice.target.wizard().then(|| match &choice.inspect {
                Loadable::Idle | Loadable::Loading => self.spinner_line(
                    "plugin-inspect-spinner".into(),
                    "Checking what works in Wizard…",
                    theme,
                    cx,
                ),
                Loadable::Ready(plugin) => div()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(support_line(plugin)))
                    .into_any_element(),
                Loadable::Error(error) => div()
                    .text_color(theme.warning_muted.opacity(0.9))
                    .child(SharedString::from(format!(
                        "Couldn't check it in Wizard: {error}"
                    )))
                    .into_any_element(),
            });
        let nothing_for_wizard = matches!(&choice.inspect, Loadable::Ready(plugin)
            if plugin.support().0.is_empty());
        // Wizard refuses a package it can run nothing of, so neither Wizard
        // choice can go ahead.
        let blocked = choice.target.wizard() && nothing_for_wizard;
        div()
            .px(px(16.0))
            .py(px(12.0))
            .border_t_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .text_size(crate::typography::ui_rems(12.5))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_color(theme.text)
                            .child(SharedString::from("Install for")),
                    )
                    .children(pills),
            )
            .when(self.wizard_ready(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .text_color(theme.text_muted)
                        .child(widgets::badge(theme, "Beta"))
                        .child(div().min_w_0().child(SharedString::from(BETA_LINE))),
                )
            })
            .children(wizard_note)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        widgets::ghost_action(theme)
                            .id("plugin-choice-cancel")
                            .hover(|s| widgets::ghost_hover(theme, s))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_choice(cx)))
                            .child(SharedString::from("Cancel")),
                    )
                    .child(
                        popover::btn_primary(theme, "Install")
                            .id("plugin-choice-install")
                            .when(blocked, |el| el.opacity(0.4).cursor_default())
                            .when(!blocked, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.confirm_choice(cx)))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_gallery(
        &self,
        installed: &[InstalledRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // "Installed" means installed everywhere it could go; a package only
        // Pi has still offers Install, so it can be added to Wizard.
        let (pi, wizard) = (self.pi_ready(), self.wizard_ready());
        let complete: HashSet<&str> = installed
            .iter()
            .filter(|row| (!pi || row.pi.is_some()) && (!wizard || row.wizard.is_some()))
            .map(|row| row.key.as_str())
            .collect();
        let header = div()
            .mt(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(widgets::field_label(theme, "Gallery"))
            .child(
                widgets::ghost_action(theme)
                    .id("plugins-gallery-open-site")
                    .flex_none()
                    .hover(|s| widgets::ghost_hover(theme, s))
                    .on_click(|_, _, cx| cx.open_url(GALLERY_PAGE_URL))
                    .child(SharedString::from("pi.dev/packages"))
                    .child(
                        icon(icons::ARROW_UP_RIGHT)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    ),
            );
        let search = div()
            .mt(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                popover::dialog_field(self.search.clone().into_any_element())
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0)),
            );
        let body: AnyElement = match &self.gallery {
            Loadable::Idle | Loadable::Loading => widgets::section_card(theme)
                .mt(px(10.0))
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "plugins-gallery-skeleton",
                    theme,
                    4,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => div()
                .child(widgets::error_strip(
                    theme,
                    format!("Couldn't load the gallery — {message}"),
                ))
                .child(
                    widgets::ghost_action(theme)
                        .id("plugins-gallery-retry")
                        .mt(px(8.0))
                        .hover(|s| widgets::ghost_hover(theme, s))
                        .on_click(cx.listener(|this, _, _, cx| this.search_gallery(true, cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            Loadable::Ready(results) if results.is_empty() => widgets::section_card(theme)
                .mt(px(10.0))
                .child(
                    widgets::card_row(theme, true).child(
                        div()
                            .text_size(crate::typography::ui_rems(widgets::ROW_DESCRIPTION_SIZE))
                            .text_color(theme.text_muted.opacity(0.8))
                            .child(SharedString::from("No packages match that search.")),
                    ),
                )
                .into_any_element(),
            Loadable::Ready(results) => widgets::section_card(theme)
                .mt(px(10.0))
                .children(results.iter().enumerate().map(|(ix, result)| {
                    self.gallery_row(
                        ix,
                        result,
                        complete.contains(result.name.as_str()),
                        theme,
                        cx,
                    )
                }))
                .into_any_element(),
        };
        div()
            .child(header)
            .child(search)
            .child(body)
            .into_any_element()
    }

    fn gallery_row(
        &self,
        ix: usize,
        package: &GalleryPackage,
        installed: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let source = format!("npm:{}", package.name);
        let installing =
            matches!(&self.pending, Some(Pending::Install { source: s, .. }) if *s == source);
        let choosing = self
            .choice
            .as_ref()
            .filter(|choice| !choice.typed && choice.source == source);
        let mut meta = Vec::new();
        if let Some(description) = &package.description {
            meta.push(
                div()
                    .min_w_0()
                    .child(SharedString::from(description.clone()))
                    .into_any_element(),
            );
        }
        if let Some(publisher) = &package.publisher {
            meta.push(
                div()
                    .child(SharedString::from(format!("by {publisher}")))
                    .into_any_element(),
            );
        }
        if let Some(downloads) = package.weekly_downloads {
            meta.push(
                div()
                    .child(SharedString::from(format!(
                        "{}/wk",
                        compact_count(downloads)
                    )))
                    .into_any_element(),
            );
        }
        if installing {
            meta.push(self.spinner_line(
                format!("plugins-gallery-spinner-{ix}"),
                "Installing…",
                theme,
                cx,
            ));
        }
        let url = package.url.clone();
        let row = widgets::card_row(theme, ix == 0)
            .id(("plugins-gallery-row", ix))
            .items_start()
            .child(widgets::row_tile(theme, icons::WIDGET))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .min_w_0()
                            .child(widgets::row_title(theme, package.name.clone()))
                            .child(widgets::badge(theme, format!("v{}", package.version))),
                    )
                    .child(widgets::meta_line(theme, meta)),
            )
            .child(
                div()
                    .id(("plugins-gallery-link", ix))
                    .flex_none()
                    .size(px(28.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|s| widgets::ghost_hover(theme, s))
                    .role(gpui::Role::Link)
                    .aria_label("View on npm")
                    .on_click(move |_, _, cx| cx.open_url(&url))
                    .child(
                        icon(icons::ARROW_UP_RIGHT)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .map(|el| {
                if installed {
                    el.child(widgets::badge_active(theme, "Installed"))
                } else if installing || choosing.is_some() {
                    el
                } else {
                    el.child(self.action(
                        theme,
                        ("plugins-gallery-install", ix),
                        "Install",
                        move |this, cx| this.begin_install(source.clone(), false, cx),
                        cx,
                    ))
                }
            });
        div()
            .flex()
            .flex_col()
            .child(row)
            .when_some(choosing, |el, choice| {
                el.child(self.render_choice(choice, theme, cx))
            })
            .into_any_element()
    }

    fn render_source_install(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let installing_typed = matches!(&self.pending, Some(Pending::Install { source, .. })
            if !source.starts_with("npm:") || !matches!(&self.gallery, Loadable::Ready(list)
                if list.iter().any(|p| source == &format!("npm:{}", p.name))));
        let choosing = self.choice.as_ref().filter(|choice| choice.typed);
        div()
            .child(
                div()
                    .mt(px(28.0))
                    .child(widgets::field_label(theme, "Install from a source")),
            )
            .child(
                div()
                    .mt(px(10.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        popover::dialog_field(self.source.clone().into_any_element())
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(if installing_typed {
                        self.spinner_line("plugins-source-spinner".into(), "Installing…", theme, cx)
                    } else {
                        self.action(
                            theme,
                            "plugins-source-install",
                            "Install",
                            |this, cx| this.install_from_source(cx),
                            cx,
                        )
                    }),
            )
            .when_some(choosing, |el, choice| {
                el.child(
                    widgets::section_card(theme)
                        .mt(px(10.0))
                        .child(self.render_choice(choice, theme, cx)),
                )
            })
            .child(
                div()
                    .mt(px(6.0))
                    .text_size(crate::typography::ui_rems(widgets::ROW_DESCRIPTION_SIZE))
                    .text_color(theme.text_muted.opacity(0.65))
                    .child(SharedString::from(
                        "An npm package name, npm:@scope/name@version, git:github.com/owner/repo@tag, a git URL, or a local path on the selected device.",
                    )),
            )
            .into_any_element()
    }
}

impl popover::ScrollRailHost for PluginsPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for PluginsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let settled = !matches!(self.packages, Loadable::Idle | Loadable::Loading)
            && !matches!(self.wizard, Loadable::Idle | Loadable::Loading);
        let installed = merge_installed(self.pi_packages(), self.wizard_plugins());
        let (pi, wizard) = (self.pi_ready(), self.wizard_ready());
        let count = (settled && (pi || wizard)).then_some(installed.len());
        let wizard_warning: Option<String> = match self.wizard.ready() {
            Some(list) if list.wizard_installed && !list.supported => Some(
                "This device's Wizard is too old for Pi plugins. Update Wizard to install them \
                 for it too."
                    .to_string(),
            ),
            Some(list) => list.error.clone(),
            None => None,
        };
        let body: AnyElement = match &self.packages {
            _ if !settled => widgets::section_card(&theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "plugins-skeleton",
                    &theme,
                    3,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) if !wizard => div()
                .child(widgets::error_strip(&theme, message.clone()))
                .child(
                    widgets::ghost_action(&theme)
                        .id("plugins-retry")
                        .mt(px(8.0))
                        .hover(|s| widgets::ghost_hover(&theme, s))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.load(cx);
                            cx.notify();
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            Loadable::Ready(packages) if !packages.pi_installed && !wizard => {
                let packages = packages.clone();
                self.render_install_pi(&packages, false, &theme, cx)
            }
            packages => {
                let install_pi = packages
                    .ready()
                    .filter(|p| !p.pi_installed)
                    .cloned()
                    .map(|p| self.render_install_pi(&p, true, &theme, cx));
                let settings_error = match packages {
                    Loadable::Error(message) => {
                        Some(format!("Couldn't read Pi's packages — {message}"))
                    }
                    other => other.ready().and_then(|p| p.settings_error.clone()),
                };
                div()
                    .children(install_pi)
                    .when_some(settings_error, |el, message| {
                        el.child(widgets::warning_strip(&theme, message))
                    })
                    .child(self.render_installed(&installed, &theme, cx))
                    .child(self.render_gallery(&installed, &theme, cx))
                    .child(self.render_source_install(&theme, cx))
                    .into_any_element()
            }
        };
        let error = self
            .error
            .clone()
            .map(|message| widgets::error_strip(&theme, message).into_any_element());
        let notice = self.notice.clone().map(|message| {
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.success_muted.opacity(0.9))
                .child(icon(icons::CHECK).size(px(14.0)).flex_none())
                .child(SharedString::from(message))
                .into_any_element()
        });
        let switcher = self.render_device_switcher(&theme, cx);
        let scrollbar = popover::rail(self, "plugins-page-scrollbar", &theme, cx);

        div()
            .id("plugins-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                div()
                    .id("plugins-page")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll.scroll)
                    .child(
                        widgets::page_column()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(widgets::page_header(&theme, "Plugins", count))
                                    .child(switcher),
                            )
                            .child(
                                widgets::page_subtitle(
                                    &theme,
                                    if wizard || !settled {
                                        "Install Pi packages (extensions, skills, prompt \
                                         templates, and themes) for Pi, for Wizard, or both, on \
                                         the selected device. New chats load them automatically."
                                    } else {
                                        "Add extensions, skills, prompt templates, and themes to \
                                         the Pi agent on the selected device. New Pi chats load \
                                         them automatically."
                                    },
                                )
                                .max_w(px(512.0))
                                .line_height(px(20.0)),
                            )
                            .when(settled && wizard, |el| el.child(self.beta_line(&theme)))
                            .when_some(wizard_warning.filter(|_| settled), |el, message| {
                                el.child(widgets::warning_strip(&theme, message))
                            })
                            .children(error)
                            .children(notice)
                            .child(body)
                            .child(
                                div()
                                    .mt(px(20.0))
                                    .flex()
                                    .flex_row()
                                    .items_start()
                                    .gap(px(8.0))
                                    .text_size(crate::typography::ui_rems(11.5))
                                    .line_height(px(17.0))
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(
                                        icon(icons::INFO_CIRCLE)
                                            .mt(px(1.0))
                                            .size(px(14.0))
                                            .flex_none(),
                                    )
                                    .child(SharedString::from(
                                        "Pi packages run with full access to the device, and \
                                         their skills steer the agent. Only install packages \
                                         you trust, and review third-party source first.",
                                    )),
                            ),
                    ),
            )
            .children(scrollbar)
    }
}

fn package_label(source: &str) -> String {
    package_key(source)
}

fn success_notice(pending: &Pending, wizard: Option<&WizardPlugin>) -> String {
    match pending {
        Pending::InstallPi => "Pi is installed.".into(),
        Pending::Install { source, target } => {
            let name = package_label(source);
            let whom = match target {
                InstallTarget::Both => "Pi and Wizard",
                InstallTarget::Wizard => "Wizard",
                InstallTarget::Pi => "Pi",
            };
            let mut text = format!("Installed {name} for {whom}. New chats load it.");
            if let Some(plugin) = wizard {
                text.push(' ');
                text.push_str(&support_line(plugin));
            }
            text
        }
        Pending::RemovePi(source) => format!("Removed {} from Pi.", package_label(source)),
        Pending::RemoveWizard(name) => format!("Removed {name} from Wizard."),
        Pending::Update(Some(source)) => format!("Updated {}.", package_label(source)),
        Pending::Update(None) => "Updated all Pi packages.".into(),
    }
}

/// What failed, and what had already gone through before it did.
fn failure_notice(pending: &Pending, done: &[&str], error: &str) -> String {
    match pending {
        Pending::InstallPi => format!("Pi installation failed — {error}"),
        Pending::Install { source, target } => {
            let name = package_label(source);
            let wizard_done = done.contains(&methods::INSTALL_WIZARD_PLUGIN);
            match (target, wizard_done) {
                (InstallTarget::Both, true) => {
                    format!("Installed {name} for Wizard, but Pi's install failed — {error}")
                }
                (InstallTarget::Pi, _) => format!("Install for Pi failed — {error}"),
                _ => format!("Install for Wizard failed — {error}"),
            }
        }
        Pending::RemovePi(_) => format!("Remove from Pi failed — {error}"),
        Pending::RemoveWizard(_) => format!("Remove from Wizard failed — {error}"),
        Pending::Update(_) => format!("Update failed — {error}"),
    }
}

/// `337311` → `337k`, `1126822` → `1.1M`.
fn compact_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}k", n / 1_000),
        _ => {
            let tenths = n / 100_000;
            if tenths % 10 == 0 {
                format!("{}M", tenths / 10)
            } else {
                format!("{}.{}M", tenths / 10, tenths % 10)
            }
        }
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    objects: Vec<SearchObject>,
}

#[derive(Deserialize)]
struct SearchObject {
    package: SearchPackage,
    #[serde(default)]
    downloads: Option<SearchDownloads>,
}

#[derive(Deserialize)]
struct SearchDownloads {
    #[serde(default)]
    weekly: Option<u64>,
}

#[derive(Deserialize)]
struct SearchPackage {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    publisher: Option<SearchPublisher>,
    #[serde(default)]
    links: SearchLinks,
}

#[derive(Deserialize)]
struct SearchPublisher {
    #[serde(default)]
    username: Option<String>,
}

#[derive(Deserialize, Default)]
struct SearchLinks {
    #[serde(default)]
    npm: Option<String>,
}

fn gallery_from(response: SearchResponse) -> Vec<GalleryPackage> {
    response
        .objects
        .into_iter()
        .map(|object| {
            let package = object.package;
            let url = package
                .links
                .npm
                .unwrap_or_else(|| format!("https://www.npmjs.com/package/{}", package.name));
            GalleryPackage {
                description: package
                    .description
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty()),
                publisher: package.publisher.and_then(|p| p.username),
                weekly_downloads: object.downloads.and_then(|d| d.weekly),
                version: package.version,
                name: package.name,
                url,
            }
        })
        .collect()
}

/// npm registry search over packages tagged `pi-package` (the pi.dev
/// gallery's own index), best matches first.
async fn fetch_gallery(query: &str) -> Result<Vec<GalleryPackage>, String> {
    let text = if query.is_empty() {
        "keywords:pi-package".to_string()
    } else {
        format!("keywords:pi-package {query}")
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(GALLERY_SEARCH_URL)
        .query(&[("text", text.as_str()), ("size", GALLERY_SIZE)])
        .send()
        .await
        .map_err(|e| format!("couldn't reach the npm registry ({e})"))?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let body: SearchResponse = response.json().await.map_err(|e| e.to_string())?;
    Ok(gallery_from(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_engine::wizard_plugins::WizardPluginItem;

    fn item(kind: &str, status: &str) -> WizardPluginItem {
        WizardPluginItem {
            kind: kind.into(),
            name: format!("{kind}-x"),
            status: status.into(),
            reason: None,
            notes: Vec::new(),
            path: None,
            loaded: None,
        }
    }

    fn plugin(name: &str, source: &str, items: Vec<WizardPluginItem>) -> WizardPlugin {
        WizardPlugin {
            name: name.into(),
            source: source.into(),
            version: Some("1.0.0".into()),
            description: None,
            items,
            notes: Vec::new(),
        }
    }

    fn pi_package(source: &str, name: &str, kind: PiPackageKind) -> PiPackage {
        PiPackage {
            source: source.into(),
            kind,
            name: name.into(),
            version: None,
            description: None,
            filtered: false,
        }
    }

    #[test]
    fn compact_counts_read_naturally() {
        assert_eq!(compact_count(12), "12");
        assert_eq!(compact_count(337_311), "337k");
        assert_eq!(compact_count(1_126_822), "1.1M");
        assert_eq!(compact_count(2_000_000), "2M");
    }

    #[test]
    fn gallery_parses_registry_search_hits() {
        let body = r#"{"objects":[{"downloads":{"monthly":1126822,"weekly":337311},
            "package":{"name":"pi-mcp-adapter","version":"2.38.0",
            "description":" MCP adapter extension for Pi ","publisher":{"username":"nicopreme"},
            "links":{"npm":"https://www.npmjs.com/package/pi-mcp-adapter"}}},
            {"package":{"name":"@s/p","version":"0.1.0"}}],"total":2}"#;
        let list = gallery_from(serde_json::from_str(body).unwrap());
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "pi-mcp-adapter");
        assert_eq!(
            list[0].description.as_deref(),
            Some("MCP adapter extension for Pi")
        );
        assert_eq!(list[0].publisher.as_deref(), Some("nicopreme"));
        assert_eq!(list[0].weekly_downloads, Some(337_311));
        assert_eq!(list[1].url, "https://www.npmjs.com/package/@s/p");
        assert_eq!(list[1].weekly_downloads, None);
    }

    #[test]
    fn install_targets_follow_the_agents_on_the_device() {
        use InstallTarget::*;
        assert_eq!(target_options(true, true), vec![Both, Wizard, Pi]);
        assert_eq!(target_options(false, true), vec![Wizard]);
        assert_eq!(target_options(true, false), vec![Pi]);
        assert!(target_options(false, false).is_empty());

        // Both agents and a fresh package: both, the default the page asks for.
        assert_eq!(default_target(true, true, false, false), Some(Both));
        // Whichever side is still missing it.
        assert_eq!(default_target(true, true, true, false), Some(Wizard));
        assert_eq!(default_target(true, true, false, true), Some(Pi));
        assert_eq!(default_target(true, true, true, true), Some(Both));
        assert_eq!(default_target(false, true, false, false), Some(Wizard));
        assert_eq!(default_target(true, false, false, false), Some(Pi));
        assert_eq!(default_target(false, false, false, false), None);

        assert!(Both.wizard() && Both.pi());
        assert!(Wizard.wizard() && !Wizard.pi());
        assert!(!Pi.wizard() && Pi.pi());
    }

    #[test]
    fn the_support_line_says_what_works_and_what_does_not() {
        let mixed = plugin(
            "p",
            "npm:p",
            vec![
                item("skill", "ready"),
                item("skill", "ready"),
                item("prompt", "installed"),
                item("extension", "unsupported"),
            ],
        );
        assert_eq!(
            support_line(&mixed),
            "In Wizard: 2 skills, 1 prompt. Not supported yet: 1 extension."
        );
        let only_ext = plugin("p", "npm:p", vec![item("extension", "unsupported")]);
        assert_eq!(
            support_line(&only_ext),
            "Nothing in it runs in Wizard yet (1 extension)."
        );
        let skills = plugin("p", "npm:p", vec![item("skill", "installed")]);
        assert_eq!(support_line(&skills), "In Wizard: 1 skill.");
    }

    #[test]
    fn installed_rows_line_wizard_up_with_pi() {
        let pi = vec![
            pi_package("npm:@s/both@^1", "@s/both", PiPackageKind::Npm),
            pi_package(
                "git:github.com/o/r@v1",
                "github.com/o/r",
                PiPackageKind::Git,
            ),
            pi_package("npm:pi-only", "pi-only", PiPackageKind::Npm),
        ];
        let wizard = vec![
            plugin("@s/both", "npm:@s/both", vec![item("skill", "installed")]),
            // Git packages match on the repo, whatever package.json calls it.
            plugin("repo-pkg", "git:github.com/o/r", vec![]),
            plugin("wiz-only", "npm:wiz-only", vec![]),
        ];
        let rows = merge_installed(&pi, &wizard);
        let shape: Vec<(&str, bool, bool)> = rows
            .iter()
            .map(|r| (r.key.as_str(), r.pi.is_some(), r.wizard.is_some()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("@s/both", true, true),
                ("github.com/o/r", true, true),
                ("pi-only", true, false),
                ("wiz-only", false, true),
            ]
        );
        assert_eq!(rows[1].name(), "repo-pkg");
        assert_eq!(rows[2].name(), "pi-only");
    }

    #[test]
    fn notices_name_the_package_and_the_target() {
        let skills = plugin("pi-tools", "npm:pi-tools", vec![item("skill", "installed")]);
        assert_eq!(
            success_notice(
                &Pending::Install {
                    source: "npm:pi-tools".into(),
                    target: InstallTarget::Both,
                },
                Some(&skills)
            ),
            "Installed pi-tools for Pi and Wizard. New chats load it. In Wizard: 1 skill."
        );
        assert_eq!(
            success_notice(&Pending::RemoveWizard("pi-tools".into()), None),
            "Removed pi-tools from Wizard."
        );
        assert_eq!(
            failure_notice(
                &Pending::Install {
                    source: "npm:pi-tools".into(),
                    target: InstallTarget::Both,
                },
                &[methods::INSTALL_WIZARD_PLUGIN],
                "boom"
            ),
            "Installed pi-tools for Wizard, but Pi's install failed — boom"
        );
        assert_eq!(
            failure_notice(&Pending::RemovePi("git:github.com/o/r".into()), &[], "boom"),
            "Remove from Pi failed — boom"
        );
    }
}
