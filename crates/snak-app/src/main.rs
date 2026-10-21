mod app;
mod code_blocks;
mod log;
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
        OpenLog,
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
    // Aborted when the caller stops waiting: a replaced sign-in task must
    // stop polling Entra, and `end_session` must cancel requests in flight.
    // Blocking store work runs on spawn_blocking, which an abort doesn't cut.
    struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
    impl<T> Drop for AbortOnDrop<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut handle = AbortOnDrop(runtime.spawn(future));
    async move {
        (&mut handle.0)
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

// Lucide icons Snak uses beyond GPUI Kit's default set.
gpui_kit::assets::icon_assets!(ExtraIcons, [Users, Phone]);

/// GPUI Kit's default icons, then [`ExtraIcons`].
struct SnakAssets;

impl AssetSource for SnakAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        // The default source reports a path it doesn't embed as an error, not
        // as None, so either way falls through to the extras.
        match gpui_kit::assets::Assets.load(path) {
            Ok(Some(bytes)) => Ok(Some(bytes)),
            _ => ExtraIcons.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path).unwrap_or_default();
        paths.extend(ExtraIcons.list(path)?);
        Ok(paths)
    }
}

/// The first window's size; `SNAK_WINDOW_SIZE=1440x900` overrides it, which
/// `scripts/screenshot.sh` uses for a consistent capture.
fn window_size() -> Size<Pixels> {
    const SNAK_WINDOW_SIZE: &str = "SNAK_WINDOW_SIZE";
    std::env::var(SNAK_WINDOW_SIZE)
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            let side = |s: &str| {
                s.parse::<f32>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 320.)
            };
            Some(size(px(side(w)?), px(side(h)?)))
        })
        .unwrap_or_else(|| size(px(1280.), px(820.)))
}

fn main() {
    log::init();
    let connector = connector();
    gpui_kit::application()
        .with_assets(SnakAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::install(cx);
            cx.set_global(settings::Settings::load());
            settings::Settings::apply(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_action(|_: &OpenLog, cx| {
                if let Some(path) = log::path() {
                    cx.open_with_system(&path);
                }
            });
            cx.set_menus(vec![Menu {
                name: "Snak".into(),
                items: vec![
                    MenuItem::action("Command Palette…", CommandPalette),
                    MenuItem::action("Settings…", OpenSettings),
                    MenuItem::action("Open Log File", OpenLog),
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
                window_bounds: Some(WindowBounds::centered(window_size(), cx)),
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
