//! The settings window, laid out like Zed's: pages on the left, one row per
//! setting with a description and a reset-to-default control.

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar,
    button::Button,
    setting::{
        NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage,
        Settings as SettingsView,
    },
    v_flex,
};
use gpui_kit::*;

use crate::app::Snak;
use crate::settings::{Appearance, LoginStorage, Settings, onepassword_accounts};
use crate::theme::InstalledThemes;

/// The open settings window, so a second ⌘, focuses it instead of opening another.
struct OpenSettings(Option<AnyWindowHandle>);

impl Global for OpenSettings {}

pub struct SettingsWindow {
    focus: FocusHandle,
    main: WeakEntity<Snak>,
    /// Looked up once per window; `op account list` takes a moment.
    onepassword_accounts: Vec<(String, String)>,
}

impl SettingsWindow {
    /// Opens the window, or focuses it if it's already open.
    pub fn open(main: WeakEntity<Snak>, cx: &mut App) {
        if let Some(handle) = cx.try_global::<OpenSettings>().and_then(|o| o.0)
            && handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(860.), px(640.)), cx)),
            window_min_size: Some(size(px(640.), px(420.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Snak Settings".into()),
                ..TitleBar::title_bar_options()
            }),
            ..TitleBar::window_options()
        };
        if let Ok((handle, _)) = gpui_kit::open_window(options, cx, |_, cx| {
            cx.new(|cx| SettingsWindow {
                focus: cx.focus_handle(),
                main,
                onepassword_accounts: onepassword_accounts(),
            })
        }) {
            cx.set_global(OpenSettings(Some(handle)));
        }
    }

    fn pages(&self, cx: &mut Context<Self>) -> Vec<SettingPage> {
        let defaults = Settings::default();
        let installed = cx.global::<InstalledThemes>();
        let options = |dark: bool| -> Vec<(SharedString, SharedString)> {
            installed
                .themes
                .iter()
                .filter(|(_, is_dark)| *is_dark == dark)
                .map(|(name, _)| (name.clone().into(), name.clone().into()))
                .collect()
        };
        let (dark_options, light_options) = (options(true), options(false));
        let (zed_dark, zed_light) = (installed.zed_dark.clone(), installed.zed_light.clone());
        let main = self.main.clone();
        let main_for_storage = self.main.clone();
        let onepassword_accounts = self.onepassword_accounts.clone();
        let signed_in = main
            .upgrade()
            .and_then(|m| m.read(cx).signed_in_as())
            .unwrap_or_else(|| "Not signed in".into());
        let service = main
            .upgrade()
            .map(|m| m.read(cx).service_name())
            .unwrap_or_default();

        vec![
            SettingPage::new("Appearance")
                .icon(Icon::new(IconName::Palette))
                .default_open(true)
                .resettable(true)
                .groups(vec![
                    SettingGroup::new().title("Theme").items(vec![
                        SettingItem::new(
                            "Mode",
                            SettingField::dropdown(
                                Appearance::ALL
                                    .iter()
                                    .map(|a| (a.label().into(), a.label().into()))
                                    .collect(),
                                |cx: &App| Settings::get(cx).appearance.label().into(),
                                |value: SharedString, cx: &mut App| {
                                    Settings::update(cx, |s| {
                                        s.appearance = Appearance::from_label(&value)
                                    })
                                },
                            )
                            .default_value(defaults.appearance.label().to_string()),
                        )
                        .description("System follows macOS light and dark mode."),
                        SettingItem::new(
                            "Dark theme",
                            SettingField::dropdown(
                                dark_options,
                                |cx: &App| Settings::get(cx).dark_theme_name(cx).into(),
                                |value: SharedString, cx: &mut App| {
                                    Settings::update(cx, |s| s.dark_theme = Some(value.to_string()))
                                },
                            )
                            .default_value(zed_dark),
                        )
                        .description(
                            "Any Zed theme works: Snak reads ~/.config/zed/themes and Zed's \
                             installed theme extensions. Defaults to the theme picked in Zed.",
                        ),
                        SettingItem::new(
                            "Light theme",
                            SettingField::dropdown(
                                light_options,
                                |cx: &App| Settings::get(cx).light_theme_name(cx).into(),
                                |value: SharedString, cx: &mut App| {
                                    Settings::update(cx, |s| {
                                        s.light_theme = Some(value.to_string())
                                    })
                                },
                            )
                            .default_value(zed_light),
                        ),
                    ]),
                    SettingGroup::new().title("Text").item(
                        SettingItem::new(
                            "UI font size",
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 11.,
                                    max: 22.,
                                    ..Default::default()
                                },
                                |cx: &App| Settings::get(cx).ui_font_size,
                                |value: f64, cx: &mut App| {
                                    Settings::update(cx, |s| s.ui_font_size = value)
                                },
                            )
                            .default_value(defaults.ui_font_size),
                        )
                        .description("Size of sidebar and message text, in pixels."),
                    ),
                ]),
            SettingPage::new("Sidebar")
                .icon(Icon::new(IconName::PanelLeft))
                .resettable(true)
                .group(SettingGroup::new().title("Conversations").items(vec![
                    SettingItem::new(
                        "Direct messages shown",
                        SettingField::number_input(
                            NumberFieldOptions {
                                min: 5.,
                                max: 500.,
                                ..Default::default()
                            },
                            |cx: &App| Settings::get(cx).dm_limit,
                            |value: f64, cx: &mut App| Settings::update(cx, |s| s.dm_limit = value),
                        )
                        .default_value(defaults.dm_limit),
                    )
                    .description("Most recent first. Search (⌘K) always reaches all of them."),
                    SettingItem::new(
                        "Team names on loose channels",
                        SettingField::switch(
                            |cx: &App| Settings::get(cx).show_team_names,
                            |value: bool, cx: &mut App| {
                                Settings::update(cx, |s| s.show_team_names = value)
                            },
                        )
                        .default_value(defaults.show_team_names),
                    )
                    .description(
                        "Show which team a channel belongs to when it sits in Starred or \
                         one of your sections.",
                    ),
                ])),
            SettingPage::new("Account")
                .icon(Icon::new(IconName::CircleUser))
                .group(SettingGroup::new().title(service).items(vec![
                    SettingItem::new(
                        "Signed in as",
                        SettingField::render(move |_, _, cx| {
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(signed_in.clone())
                        }),
                    ),
                    SettingItem::new(
                        "Sign out",
                        SettingField::render(move |options, _, _| {
                            let main = main.clone();
                            Button::new("sign-out")
                                .outline()
                                .with_size(options.size())
                                .label("Sign out")
                                .on_click(move |_, _, cx| {
                                    main.update(cx, |snak, cx| snak.sign_out(cx)).ok();
                                })
                        }),
                    )
                    .description("Removes the saved login."),
                    SettingItem::new(
                        "Keep login in",
                        SettingField::dropdown(
                            LoginStorage::ALL
                                .iter()
                                .map(|s| (s.label().into(), s.label().into()))
                                .collect(),
                            |cx: &App| Settings::get(cx).login_storage.label().into(),
                            {
                                let main = main_for_storage.clone();
                                move |value: SharedString, cx: &mut App| {
                                    Settings::update(cx, |s| {
                                        s.login_storage = LoginStorage::from_label(&value)
                                    });
                                    main.update(cx, |snak, cx| snak.apply_credential_storage(cx))
                                        .ok();
                                }
                            },
                        )
                        .default_value(LoginStorage::Keychain.label().to_string()),
                    )
                    .description(
                        "Keychain prompts after every rebuild of an unsigned dev build; \
                         scripts/dev-cert.sh fixes that. 1Password asks for Touch ID when \
                         locked. A file never prompts, but any program running as you can \
                         read it and use your session.",
                    ),
                    SettingItem::new(
                        "1Password account",
                        SettingField::dropdown(
                            onepassword_accounts
                                .iter()
                                .map(|(url, email)| {
                                    (url.clone().into(), format!("{email} ({url})").into())
                                })
                                .collect(),
                            |cx: &App| {
                                Settings::get(cx)
                                    .onepassword_account
                                    .clone()
                                    .unwrap_or_default()
                                    .into()
                            },
                            {
                                let main = main_for_storage.clone();
                                move |value: SharedString, cx: &mut App| {
                                    Settings::update(cx, |s| {
                                        s.onepassword_account = Some(value.to_string())
                                    });
                                    main.update(cx, |snak, cx| snak.apply_credential_storage(cx))
                                        .ok();
                                }
                            },
                        ),
                    )
                    .description(if onepassword_accounts.is_empty() {
                        "Install the 1Password CLI (op) and turn on its desktop app \
                         integration to use 1Password."
                    } else {
                        "Which account holds the \"Snak Teams login\" item. Used when \
                         \"Keep login in\" is 1Password."
                    }),
                ])),
            SettingPage::new("About")
                .icon(Icon::new(IconName::Info))
                .group(SettingGroup::new().title("Snak").items(vec![
                    SettingItem::new(
                        "Version",
                        SettingField::render(|_, _, _| {
                            div().text_sm().child(env!("CARGO_PKG_VERSION"))
                        }),
                    ),
                    SettingItem::new(
                        "License",
                        SettingField::render(|_, _, _| div().text_sm().child("GPL-3.0-or-later")),
                    )
                    .description(
                        "The Teams backend is MPL-2.0, derived from Squads. The bundled themes are \
                         MIT: One, Ayu, Gruvbox, Catppuccin, Rosé Pine, Dracula and Tokyo Night.",
                    ),
                ])),
        ]
    }
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = self.pages(cx);
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(TitleBar::new().child(div().text_sm().child("Settings")))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(SettingsView::new("snak-settings").small().pages(pages)),
            )
    }
}
