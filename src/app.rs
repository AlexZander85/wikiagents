use anyhow::Result;
use std::net::TcpListener;
use std::thread;
use tao::{
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

/// Run the native desktop application window.
pub fn run(preferred_port: Option<u16>) -> Result<()> {
    let port = preferred_port.unwrap_or(7575);
    let addr = format!("127.0.0.1:{port}");

    // Bind first, open the index later. Binding is instant either way: it
    // succeeds, and the WebView's first request waits in the accept queue
    // while the server thread opens the index; or the port is taken, which
    // means another AgentWiki already serves it. Probing with a connect
    // instead cost two seconds on Windows, which retries a refused loopback
    // connection before giving up - on every cold start.
    match TcpListener::bind(&addr) {
        Ok(listener) => {
            thread::spawn(move || {
                if let Err(e) = crate::web::serve_listener(listener) {
                    eprintln!("web: {e:#}");
                }
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
        Err(e) => return Err(e.into()),
    }

    let event_loop = EventLoop::new();
    let icon = {
        let rgba = include_bytes!("../assets/icon_64.rgba");
        tao::window::Icon::from_rgba(rgba.to_vec(), 64, 64).ok()
    };

    let mut builder = WindowBuilder::new()
        .with_title(format!("WikiAgents v{}", env!("CARGO_PKG_VERSION")))
        .with_inner_size(tao::dpi::LogicalSize::new(1440.0, 920.0));
    if let Some(ic) = icon {
        builder = builder.with_window_icon(Some(ic));
    }
    let window = builder.build(&event_loop)?;

    let url = format!("http://{addr}");
    // External links (window.open from the UI, e.g. "Search in Google") are
    // handed to the system browser from THIS process. Doing it via the HTTP
    // server is unreliable: the app may be attached to a server some other
    // process started (the bind above found the port taken), which may not be
    // allowed to launch a browser at all.
    let _webview = WebViewBuilder::new()
        .with_url(&url)
        .with_new_window_req_handler(|target, _features| {
            if target.starts_with("https://") || target.starts_with("http://") {
                crate::web::open_browser(&target);
            }
            wry::NewWindowResponse::Deny
        })
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => (),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => (),
        }
    });
}
