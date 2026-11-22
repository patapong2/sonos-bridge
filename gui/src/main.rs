//! sonos-bridge-gui - desktop panel for the sonos-bridge daemon.
//!
//! It is a thin client: every routing decision lives in the daemon
//! (`../src/`), this window only reads `/api/state` and posts commands to
//! `/api/cmd`. Audio keeps running when this window is closed, and the same
//! control surface is available from the CLI and the browser.
//!
//! The one thing only this app can do is *start* the daemon: a daemon that is
//! not running cannot serve a panel. See `BridgeView::start_daemon`.
//!
//! Run:  cargo run --release            (optionally: -- http://127.0.0.1:8688)

mod view;

use gpui::{px, size, App, AppContext, Application, Bounds, Entity, WindowBounds, WindowOptions,
    WindowDecorations};
use sonos_bridge_gui::base_url;
use view::BridgeView;

fn main() {
    let explicit = std::env::args().nth(1).unwrap_or_default();
    let base = base_url(&explicit);

    Application::new().run(move |cx: &mut App| {
        let model: Entity<BridgeView> = cx.new(|cx| BridgeView::new(base, cx));

        let bounds = Bounds::centered(None, size(px(980.0), px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("sonos-bridge".into()),
                    ..Default::default()
                }),
                // the compositor is told not to decorate: the app draws its own
                // titlebar (see view::titlebar), so this avoids a double bar
                window_decorations: Some(WindowDecorations::Client),
                app_id: Some("sonos-bridge".into()),
                window_min_size: Some(size(px(560.0), px(360.0))),
                ..Default::default()
            },
            |_, _| model.clone(),
        )
        .expect("could not open the sonos-bridge window");

        cx.activate(true);
    });
}
