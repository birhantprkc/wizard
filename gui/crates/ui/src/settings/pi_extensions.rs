//! Settings → Pi extensions: browse the Pi package gallery, install with one
//! click, and manage the packages the selected device's Pi loads.
//!
//! Pi packages bundle extensions, skills, prompt templates, and themes. The
//! gallery is every npm package tagged `pi-package` (what pi.dev/packages
//! lists), searched straight from the npm registry by this viewport. Package
//! state lives with Pi on the target device, so installs, removals, and
//! updates go through the engine's relay-forwardable `*PiPackage*` RPCs, and
//! the page-header device switcher (the Agents pattern) retargets them.

use std::collections::HashSet;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};
use serde::Deserialize;
use zeron_engine::pi_packages::{PiPackage, PiPackageKind, PiPackages};
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

/// The one package operation in flight — Pi serializes them anyway, and one
/// at a time keeps each row's spinner honest.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    InstallPi,
    Install(String),
    Remove(String),
    Update(Option<String>),
}

pub struct PiExtensionsPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    /// Which device's Pi is shown/edited; `None` = this device.
    target_device: Option<String>,
    device_menu_open: bool,
    device_menu_pressed_open: bool,
    packages: Loadable<PiPackages>,
    load_task: Option<Task<()>>,
    pending: Option<Pending>,
    op_task: Option<Task<()>>,
    error: Option<String>,
    notice: Option<String>,
    search: Entity<ComposerInput>,
    source: Entity<ComposerInput>,
    gallery: Loadable<Vec<GalleryPackage>>,
    gallery_query: Option<String>,
    gallery_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PiExtensionsPage {
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
            load_task: None,
            pending: None,
            op_task: None,
            error: None,
            notice: None,
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
        self.packages = Loadable::Idle;
        self.load(cx);
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({}));
        let target = self.target_device.clone();
        self.packages = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_PI_PACKAGES, params)
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
    }

    /// Run one package operation on the target device; every reply carries
    /// the refreshed package list.
    fn run(&mut self, pending: Pending, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match &pending {
            Pending::InstallPi => (
                methods::INSTALL_HARNESS,
                serde_json::json!({ "harness": HarnessId::Pi }),
            ),
            Pending::Install(source) => (
                methods::INSTALL_PI_PACKAGE,
                serde_json::json!({ "source": source }),
            ),
            Pending::Remove(source) => (
                methods::REMOVE_PI_PACKAGE,
                serde_json::json!({ "source": source }),
            ),
            Pending::Update(source) => (
                methods::UPDATE_PI_PACKAGES,
                match source {
                    Some(source) => serde_json::json!({ "source": source }),
                    None => serde_json::json!({}),
                },
            ),
        };
        let params = self.with_target(params);
        let target = self.target_device.clone();
        self.pending = Some(pending.clone());
        self.error = None;
        self.notice = None;
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|error| error.to_string());
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.pending = None;
                match result {
                    Ok(value) => match &pending {
                        // InstallHarness replies with the harness catalog;
                        // re-probe Pi and let the composer see the new agent.
                        Pending::InstallPi => {
                            crate::pickers::bump_harness_catalog(cx);
                            page.notice = Some("Pi is installed. Add extensions below.".into());
                            page.load(cx);
                        }
                        _ => match serde_json::from_value::<PiPackages>(value) {
                            Ok(list) => {
                                page.notice = Some(success_notice(&pending));
                                if matches!(pending, Pending::Install(_)) {
                                    page.source.update(cx, |input, cx| input.set_text("", cx));
                                }
                                page.packages = Loadable::Ready(list);
                            }
                            Err(error) => page.error = Some(error.to_string()),
                        },
                    },
                    Err(error) => page.error = Some(failure_notice(&pending, &error)),
                }
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
            Ok(source) => self.run(Pending::Install(source), cx),
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
                .id("pi-extensions-device-switcher")
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
                    popover::menu_row(theme, is_active, format!("pi-extensions-device-row-{ix}"))
                        .id(("pi-extensions-device-row", ix))
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
            trigger = trigger.child(popover::anchored_menu(
                "pi-extensions-device-menu",
                menu,
                None,
            ));
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

    fn render_install_pi(
        &self,
        packages: &PiPackages,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let installing = self.pending == Some(Pending::InstallPi);
        let mut meta = vec![
            div()
                .child(SharedString::from(
                    "Extensions run inside the Pi coding agent. Install Pi on this device to add them.",
                ))
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
        packages: &PiPackages,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let updating_all = self.pending == Some(Pending::Update(None));
        let header = div()
            .mt(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(widgets::field_label(theme, "Installed"))
            .when(!packages.packages.is_empty(), |el| {
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
        let card = if packages.packages.is_empty() {
            widgets::section_card(theme).mt(px(10.0)).child(
                widgets::card_row(theme, true).child(
                    div()
                        .text_size(crate::typography::ui_rems(widgets::ROW_DESCRIPTION_SIZE))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child(SharedString::from(
                            "No extensions yet. Pick one from the gallery below.",
                        )),
                ),
            )
        } else {
            widgets::section_card(theme).mt(px(10.0)).children(
                packages
                    .packages
                    .iter()
                    .enumerate()
                    .map(|(ix, package)| self.installed_row(ix, package, theme, cx)),
            )
        };
        div().child(header).child(card).into_any_element()
    }

    fn installed_row(
        &self,
        ix: usize,
        package: &PiPackage,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let removing = self.pending == Some(Pending::Remove(package.source.clone()));
        let updating = self.pending == Some(Pending::Update(Some(package.source.clone())));
        let mut meta = vec![
            div()
                .child(SharedString::from(match package.kind {
                    PiPackageKind::Npm => "npm",
                    PiPackageKind::Git => "git",
                    PiPackageKind::Local => "local",
                }))
                .into_any_element(),
        ];
        if let Some(version) = &package.version {
            meta.push(
                div()
                    .child(SharedString::from(format!("v{version}")))
                    .into_any_element(),
            );
        }
        if package.filtered {
            meta.push(
                div()
                    .child(SharedString::from("filtered"))
                    .into_any_element(),
            );
        }
        if let Some(description) = &package.description {
            meta.push(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(description.clone()))
                    .into_any_element(),
            );
        }
        if removing {
            meta.push(self.spinner_line(format!("pi-remove-spinner-{ix}"), "Removing…", theme, cx));
        }
        if updating {
            meta.push(self.spinner_line(format!("pi-update-spinner-{ix}"), "Updating…", theme, cx));
        }
        let busy = removing || updating;
        let update_source = package.source.clone();
        let remove_source = package.source.clone();
        widgets::card_row(theme, ix == 0)
            .id(("pi-installed-row", ix))
            .child(widgets::row_tile(theme, icons::WIDGET))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, package.name.clone()))
                    .child(widgets::meta_line(theme, meta)),
            )
            .when(!busy && package.kind != PiPackageKind::Local, |el| {
                el.child(self.action(
                    theme,
                    ("pi-update", ix),
                    "Update",
                    move |this, cx| this.run(Pending::Update(Some(update_source.clone())), cx),
                    cx,
                ))
            })
            .when(!busy, |el| {
                el.child(self.action(
                    theme,
                    ("pi-remove", ix),
                    "Remove",
                    move |this, cx| this.run(Pending::Remove(remove_source.clone()), cx),
                    cx,
                ))
            })
            .into_any_element()
    }

    fn render_gallery(
        &self,
        packages: &PiPackages,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let installed: HashSet<&str> = packages
            .packages
            .iter()
            .filter(|p| p.kind == PiPackageKind::Npm)
            .map(|p| p.name.as_str())
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
                    .id("pi-gallery-open-site")
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
                    "pi-gallery-skeleton",
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
                        .id("pi-gallery-retry")
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
                        installed.contains(result.name.as_str()),
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
        let installing = self.pending == Some(Pending::Install(source.clone()));
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
                format!("pi-gallery-spinner-{ix}"),
                "Installing…",
                theme,
                cx,
            ));
        }
        let url = package.url.clone();
        widgets::card_row(theme, ix == 0)
            .id(("pi-gallery-row", ix))
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
                    .id(("pi-gallery-link", ix))
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
                } else if installing {
                    el
                } else {
                    el.child(self.action(
                        theme,
                        ("pi-gallery-install", ix),
                        "Install",
                        move |this, cx| this.run(Pending::Install(source.clone()), cx),
                        cx,
                    ))
                }
            })
            .into_any_element()
    }

    fn render_source_install(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let installing_typed = matches!(&self.pending, Some(Pending::Install(source))
            if !source.starts_with("npm:") || !matches!(&self.gallery, Loadable::Ready(list)
                if list.iter().any(|p| source == &format!("npm:{}", p.name))));
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
                        self.spinner_line("pi-source-spinner".into(), "Installing…", theme, cx)
                    } else {
                        self.action(
                            theme,
                            "pi-source-install",
                            "Install",
                            |this, cx| this.install_from_source(cx),
                            cx,
                        )
                    }),
            )
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

impl popover::ScrollRailHost for PiExtensionsPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for PiExtensionsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let count = self
            .packages
            .ready()
            .filter(|p| p.pi_installed)
            .map(|p| p.packages.len());
        let body: AnyElement = match &self.packages {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "pi-extensions-skeleton",
                    &theme,
                    3,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => div()
                .child(widgets::error_strip(&theme, message.clone()))
                .child(
                    widgets::ghost_action(&theme)
                        .id("pi-extensions-retry")
                        .mt(px(8.0))
                        .hover(|s| widgets::ghost_hover(&theme, s))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.load(cx);
                            cx.notify();
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            Loadable::Ready(packages) if !packages.pi_installed => {
                let packages = packages.clone();
                self.render_install_pi(&packages, &theme, cx)
            }
            Loadable::Ready(packages) => {
                let packages = packages.clone();
                div()
                    .when_some(packages.settings_error.clone(), |el, message| {
                        el.child(widgets::warning_strip(&theme, message))
                    })
                    .child(self.render_installed(&packages, &theme, cx))
                    .child(self.render_gallery(&packages, &theme, cx))
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
        let scrollbar = popover::rail(self, "pi-extensions-page-scrollbar", &theme, cx);

        div()
            .id("pi-extensions-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                div()
                    .id("pi-extensions-page")
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
                                    .child(widgets::page_header(&theme, "Pi extensions", count))
                                    .child(switcher),
                            )
                            .child(
                                widgets::page_subtitle(
                                    &theme,
                                    "Add extensions, skills, prompt templates, and themes to the Pi agent \
                                     on the selected device. New Pi chats load them automatically.",
                                )
                                .max_w(px(512.0))
                                .line_height(px(20.0)),
                            )
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
                                        "Pi packages run with full access to the device. Only install \
                                         packages you trust, and review third-party source first.",
                                    )),
                            ),
                    ),
            )
            .children(scrollbar)
    }
}

fn success_notice(pending: &Pending) -> String {
    match pending {
        Pending::InstallPi => "Pi is installed.".into(),
        Pending::Install(source) => format!(
            "Installed {}. New Pi chats load it.",
            display_source(source)
        ),
        Pending::Remove(source) => format!("Removed {}.", display_source(source)),
        Pending::Update(Some(source)) => format!("Updated {}.", display_source(source)),
        Pending::Update(None) => "Updated all Pi packages.".into(),
    }
}

fn failure_notice(pending: &Pending, error: &str) -> String {
    let verb = match pending {
        Pending::InstallPi => return format!("Pi installation failed — {error}"),
        Pending::Install(_) => "Install",
        Pending::Remove(_) => "Remove",
        Pending::Update(_) => "Update",
    };
    format!("{verb} failed — {error}")
}

fn display_source(source: &str) -> &str {
    source.strip_prefix("npm:").unwrap_or(source)
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
    fn notices_name_the_package() {
        assert_eq!(
            success_notice(&Pending::Install("npm:pi-tools".into())),
            "Installed pi-tools. New Pi chats load it."
        );
        assert_eq!(
            failure_notice(&Pending::Remove("git:github.com/o/r".into()), "boom"),
            "Remove failed — boom"
        );
    }
}
