mod app;
mod palette;
mod prefs;
mod settings;
mod settings_window;
mod sidebar;
mod theme;

use std::future::Future;
use std::sync::{Arc, OnceLock};

use gpui_kit::*;
use snak_chat::{ChatError, Connector};

actions!(
    snak,
    [
        JumpTo,
        NextConversation,
        PreviousConversation,
        CloseThread,
        OpenSettings,
        Quit,
        CommandPalette,
        ShowUnreads,
        ToggleUnreadsOnly,
        ToggleStar,
        ToggleMute,
        ToggleHide,
        CollapseAll,
        ExpandAll,
        CycleTheme,
        SignOut,
    ]
);

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Runs network work on tokio; GPUI's executor can await the handle directly.
pub fn io<T: Send + 'static>(
    future: impl Future<Output = snak_chat::Result<T>> + Send + 'static,
) -> impl Future<Output = snak_chat::Result<T>> {
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap_or_else(|e| panic!("cannot start the network runtime: {e}"))
    });
    let handle = runtime.spawn(future);
    async move {
        handle
            .await
            .map_err(|e| ChatError::Other(format!("background task stopped: {e}")))?
    }
}

/// The only place that knows which service backs the UI.
fn connector() -> Arc<dyn Connector> {
    if std::env::args().any(|arg| arg == "--demo") {
        Arc::new(snak_demo::DemoConnector)
    } else {
        Arc::new(snak_teams::TeamsConnector::new())
    }
}

fn main() {
    let connector = connector();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::install(cx);
            cx.set_global(settings::Settings::load());
            settings::Settings::apply(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.set_menus(vec![Menu {
                name: "Snak".into(),
                items: vec![
                    MenuItem::action("Command Palette…", CommandPalette),
                    MenuItem::action("Settings…", OpenSettings),
                    MenuItem::separator(),
                    MenuItem::action("Quit Snak", Quit),
                ],
                disabled: false,
            }]);
            cx.bind_keys([
                KeyBinding::new("cmd-k", JumpTo, None),
                KeyBinding::new("cmd-,", OpenSettings, None),
                KeyBinding::new("cmd-shift-p", CommandPalette, None),
                KeyBinding::new("cmd-shift-a", ShowUnreads, None),
                KeyBinding::new("cmd-shift-u", ToggleUnreadsOnly, None),
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("alt-down", NextConversation, None),
                KeyBinding::new("alt-up", PreviousConversation, None),
                KeyBinding::new("escape", CloseThread, Some("Snak")),
            ]);

            // Snak draws its own title bar, like Zed, with the traffic lights inset.
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1280.), px(820.)), cx)),
                window_min_size: Some(size(px(720.), px(480.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Snak".into()),
                    ..gpui_kit::component::TitleBar::title_bar_options()
                }),
                ..gpui_kit::component::TitleBar::window_options()
            };
            if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| app::Snak::new(connector.clone(), window, cx))
            }) {
                eprintln!("snak: cannot open window: {e}");
                cx.quit();
            }
        });
}
