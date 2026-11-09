//! Tell the person an agent needs them: terminal bell plus a desktop
//! notification when one is available (notify-send, or a Windows toast on WSL).

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    NotifySend,
    WslToast,
    None,
}

fn backend() -> Backend {
    static B: OnceLock<Backend> = OnceLock::new();
    *B.get_or_init(|| {
        if std::env::var_os("HIERO_NO_NOTIFY").is_some() {
            Backend::None
        } else if on_path("notify-send") {
            Backend::NotifySend
        } else if is_wsl() && on_path("powershell.exe") {
            Backend::WslToast
        } else {
            Backend::None
        }
    })
}

fn on_path(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
}

/// Ring the bell and show a notification, as `[notify]` allows, for an
/// alert of kind `what` (blocked, done, pipeline, …). Never blocks.
pub fn alert(what: &str, title: &str, body: &str) {
    let cfg = &crate::config::get().notify;
    if !cfg.wants(what) {
        return;
    }
    if cfg.bell {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x07");
        let _ = out.flush();
    }
    if !cfg.desktop {
        return;
    }
    let spawned = match backend() {
        Backend::NotifySend => Command::new("notify-send")
            .args(["--app-name=hiero", title, body])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),
        Backend::WslToast => Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", TOAST_PS])
            // Text goes through the environment so nothing needs escaping.
            .env("HIERO_TOAST_TITLE", title)
            .env("HIERO_TOAST_BODY", body)
            .env("WSLENV", "HIERO_TOAST_TITLE:HIERO_TOAST_BODY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),
        Backend::None => return,
    };
    if let Ok(mut child) = spawned {
        // Reap it in the background so it doesn't linger as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

const TOAST_PS: &str = r#"
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
$t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$x = $t.GetElementsByTagName('text')
$x.Item(0).AppendChild($t.CreateTextNode($env:HIERO_TOAST_TITLE)) > $null
$x.Item(1).AppendChild($t.CreateTextNode($env:HIERO_TOAST_BODY)) > $null
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show([Windows.UI.Notifications.ToastNotification]::new($t))
"#;
