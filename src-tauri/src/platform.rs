//! Handing a finished transfer to the operating system.
//!
//! A phone keeps a file an application wrote by path to itself: no other
//! application is shown it, and nothing indexes it, which is why received files
//! are registered with the media database once they are on disk. Opening such a
//! file is the other half of the same problem, since a phone has no file
//! manager to reveal it in. Both go through the Android activity; a desktop
//! needs neither, because its file system is what file managers read and its
//! default program is what opens a file.

use std::path::Path;

use anyhow::Result;
use tauri::AppHandle;

/// Gives a finished file to the phone's media database, which is what makes it
/// show up in a file manager, a gallery and over USB. Does nothing on a
/// desktop, where those read the file system itself.
pub fn publish(app: &AppHandle, path: &Path) {
    #[cfg(target_os = "android")]
    if let Err(error) = call_activity(app, "publishReceivedFile", path) {
        tracing::warn!("cannot give the received file to the media database: {error:#}");
    }

    #[cfg(not(target_os = "android"))]
    let _ = (app, path);
}

/// Opens a file with whatever the platform opens its type with: the phone's
/// viewer for that type, or the default program on a desktop.
///
/// A phone reports the outcome itself — the activity raises a toast when no
/// application claims the file, which its `openReceivedFile` does. What reaches
/// the caller here is only whether the request could be handed over at all.
pub fn open(app: &AppHandle, path: &Path) -> Result<()> {
    #[cfg(target_os = "android")]
    return call_activity(app, "openReceivedFile", path);

    #[cfg(not(target_os = "android"))]
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_path(path.to_string_lossy(), None::<&str>)?;
        Ok(())
    }
}

/// Calls one of the methods the Android activity exposes to the interface.
///
/// It runs on the webview's thread: that is where Java expects to be called
/// from for anything that starts an activity, and it is the thread the JNI
/// environment belongs to. The call is dispatched rather than made, so a
/// failure inside it is logged instead of returned.
#[cfg(target_os = "android")]
fn call_activity(app: &AppHandle, method: &'static str, path: &Path) -> Result<()> {
    use tauri::Manager;

    let window = app
        .get_webview_window("main")
        .ok_or_else(|| anyhow::anyhow!("there is no window to run this in"))?;
    let path = path.to_string_lossy().into_owned();
    window.with_webview(move |webview| {
        webview.jni_handle().exec(move |env, activity, _webview| {
            let call = (|| -> jni::errors::Result<()> {
                let argument = env.new_string(&path)?;
                env.call_method(
                    activity,
                    method,
                    "(Ljava/lang/String;)V",
                    &[(&argument).into()],
                )?;
                Ok(())
            })();
            if let Err(error) = call {
                tracing::warn!("the activity's {method} failed: {error}");
            }
        });
    })?;
    Ok(())
}
