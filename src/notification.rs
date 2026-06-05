//! System and in-terminal notifications.
//!
//! All functions fail silently — notifications are best-effort and must never
//! block or crash the main workflow.

/// Show an OS-level system notification (macOS banner / Linux desktop popup).
///
/// `subtitle` is shown between title and body on macOS; on Linux it is
/// prepended to the body since `notify-rust` has no subtitle field.
pub fn show_system(title: &str, subtitle: Option<&str>, message: &str) {
    #[cfg(target_os = "macos")]
    {
        use mac_notification_sys::{set_application, Notification};
        if let Err(e) = set_application("com.apple.Terminal") {
            tracing::debug!("Failed to set notification application: {:?}", e);
        }
        if let Err(e) = Notification::default()
            .title(title)
            .maybe_subtitle(subtitle)
            .message(message)
            .send()
        {
            tracing::debug!("Failed to send macOS notification: {:?}", e);
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let body = match subtitle {
            Some(s) => format!("{s}\n{message}"),
            None => message.to_string(),
        };
        if let Err(e) = notify_rust::Notification::new()
            .summary(title)
            .body(&body)
            .show()
        {
            tracing::debug!("Failed to send desktop notification: {:?}", e);
        }
    }
}

/// Show a transient overlay message in the current tmux client.
///
/// Uses `tmux display-message` which renders a message in the status line for
/// `display-time` ms. Only runs when the `TMUX` environment variable is set.
pub fn show_tmux_overlay(message: &str) {
    if std::env::var_os("TMUX").is_none() {
        return;
    }

    let _ = std::process::Command::new("tmux")
        .args(["display-message", message])
        .status();
}
