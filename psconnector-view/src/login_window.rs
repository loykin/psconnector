//! PSN sign-in inside the app: a small window with the system web view (WebKit on macOS, WebView2 on
//! Windows) showing Sony's sign-in page. When the page sends the browser to the Remote Play redirect
//! address, its code is caught here, so nothing has to be copied by hand.
//!
//! It runs as its own process (`psconnector-view --login-window`), because a second native event loop
//! cannot share the main window's. It prints `REDIRECT <address>` on stdout and exits.

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn run() -> ! {
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    const REDIRECT: &str = "https://remoteplay.dl.playstation.net/remoteplay/redirect";
    /// Marks every input field (also ones added later) as not for autofill.
    const NO_AUTOFILL: &str = r#"(() => {
        const off = root => root.querySelectorAll && root.querySelectorAll('input').forEach(i => {
            i.setAttribute('autocomplete', i.type === 'password' ? 'new-password' : 'off');
        });
        const watch = () => {
            off(document);
            new MutationObserver(ms => ms.forEach(m => m.addedNodes.forEach(n => { off(n); if (n.tagName === 'INPUT') off(n.parentNode); })))
                .observe(document.documentElement, { childList: true, subtree: true });
        };
        if (document.documentElement) watch(); else document.addEventListener('DOMContentLoaded', watch);
    })();"#;

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let window = match WindowBuilder::new()
        .with_title("Sign in to PSN")
        .with_inner_size(tao::dpi::LogicalSize::new(520.0, 720.0))
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("cannot open the sign-in window: {e}");
            std::process::exit(2);
        }
    };
    // macOS only routes ⌘C / ⌘V / ⌘A to the web view through the app menu, so give the window one
    #[cfg(target_os = "macos")]
    let _menu = edit_menu();
    let proxy = event_loop.create_proxy();
    let webview = WebViewBuilder::new()
        .with_url(psconnector::psn::login_url())
        // a fresh session each time: nothing from the sign-in is kept on disk
        .with_incognito(true)
        // ask WebKit not to offer saved passwords: each lookup makes macOS ask for keychain access again
        .with_initialization_script_for_main_only(NO_AUTOFILL, false)
        .with_navigation_handler(move |url| {
            if url.starts_with(REDIRECT) {
                let _ = proxy.send_event(url);
                return false; // the redirect page itself is not needed
            }
            true
        })
        .build(&window);
    let _webview = match webview {
        Ok(w) => w,
        Err(e) => {
            eprintln!("cannot open the web view: {e}");
            std::process::exit(2);
        }
    };
    #[cfg(target_os = "macos")]
    webcrypto_key::provide(&_webview);

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(url) => {
                println!("REDIRECT {url}");
                std::process::exit(0);
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => std::process::exit(1),
            _ => {}
        }
    })
}

/// Sony's page uses WebCrypto, and by default WebKit then reads (or makes) a "WebCrypto Master Key" in the
/// login keychain. For an app without a stable signature, macOS asks for permission every time. WebKit
/// skips the keychain when the navigation delegate hands over a key itself (`_webCryptoMasterKeyForWebView:`),
/// so the sign-in window gives it a random key of its own. The session is private and keeps nothing, so
/// a new key each run is fine.
#[cfg(target_os = "macos")]
mod webcrypto_key {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::OnceLock;

    use block2::DynBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Imp, Sel};
    use objc2::{msg_send, sel};
    use objc2_foundation::NSData;
    use wry::WebViewExtMacOS;

    fn key() -> &'static [u8; 16] {
        static KEY: OnceLock<[u8; 16]> = OnceLock::new();
        KEY.get_or_init(|| {
            // RandomState is seeded from the OS random source
            let mut k = [0u8; 16];
            for half in k.chunks_mut(8) {
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_u64(std::process::id().into());
                half.copy_from_slice(&h.finish().to_le_bytes());
            }
            k
        })
    }

    extern "C-unwind" fn key_sync(_this: *mut AnyObject, _cmd: Sel, _webview: *mut AnyObject) -> *mut NSData {
        Retained::autorelease_return(NSData::with_bytes(key()))
    }

    extern "C-unwind" fn key_async(
        _this: *mut AnyObject,
        _cmd: Sel,
        _webview: *mut AnyObject,
        done: &DynBlock<dyn Fn(*mut NSData)>,
    ) {
        let data = NSData::with_bytes(key());
        done.call((Retained::as_ptr(&data) as *mut NSData,));
    }

    pub fn provide(webview: &wry::WebView) {
        let wk = webview.webview();
        unsafe {
            let delegate: *mut AnyObject = msg_send![&*wk, navigationDelegate];
            if delegate.is_null() {
                return;
            }
            let cls = (*delegate).class() as *const _ as *mut objc2::runtime::AnyClass;
            let add = |name: Sel, imp: Imp, types: &std::ffi::CStr| {
                objc2::ffi::class_addMethod(cls, name, imp, types.as_ptr());
            };
            add(
                sel!(_webCryptoMasterKeyForWebView:),
                std::mem::transmute::<*const (), Imp>(key_sync as *const ()),
                c"@@:@",
            );
            add(
                sel!(_webCryptoMasterKeyForWebView:completionHandler:),
                std::mem::transmute::<*const (), Imp>(key_async as *const ()),
                c"v@:@@?",
            );
            // WebKit checks which delegate methods exist when the delegate is set, so set it again
            let _: () = msg_send![&*wk, setNavigationDelegate: delegate];
        }
    }
}

/// App menu + Edit menu (undo, cut, copy, paste, select all). Must stay alive while the window is open.
#[cfg(target_os = "macos")]
fn edit_menu() -> Option<muda::Menu> {
    use muda::{Menu, PredefinedMenuItem as P, Submenu};
    let app = Submenu::with_items("PsConnector", true, &[&P::close_window(None), &P::quit(None)]).ok()?;
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &P::undo(None),
            &P::redo(None),
            &P::separator(),
            &P::cut(None),
            &P::copy(None),
            &P::paste(None),
            &P::select_all(None),
        ],
    )
    .ok()?;
    let menu = Menu::with_items(&[&app, &edit]).ok()?;
    menu.init_for_nsapp();
    Some(menu)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn run() -> ! {
    eprintln!("the in-app sign-in window is only available on macOS and Windows");
    std::process::exit(2);
}

/// Opens the sign-in window and waits for it. Ok(address) once signed in; Err when the window could not
/// open (use the browser instead) or was closed without signing in.
pub fn sign_in() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = std::process::Command::new(exe)
        .arg("--login-window")
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot start the sign-in window: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.lines().find_map(|l| l.strip_prefix("REDIRECT ")) {
        Some(url) => Ok(url.trim().to_string()),
        None if out.status.code() == Some(1) => Err("sign-in window closed".into()),
        None => Err("the sign-in window did not work; use the browser instead".into()),
    }
}
