//! Niri 的来源焦点事件。只订阅窗口身份，不保存标题，也不扫描输入。
//! 焦点身份不是 PRIMARY 所有者身份；两者不能混为一谈。

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncWriteExt, BufReader};

use super::{Endpoint, Focus, Update, message};

#[derive(Deserialize)]
struct Window {
    id: u64,
    is_focused: bool,
}

#[derive(Deserialize)]
enum Event {
    WindowsChanged { windows: Vec<Window> },
    WindowOpenedOrChanged { window: Window },
    WindowFocusChanged { id: Option<u64> },
    WindowClosed { id: u64 },
    // Niri 26.04 的其余事件明确识别，但不保存其载荷。
    WorkspacesChanged {},
    WorkspaceUrgencyChanged {},
    WorkspaceActivated {},
    WorkspaceActiveWindowChanged {},
    WindowFocusTimestampChanged {},
    WindowUrgencyChanged {},
    WindowLayoutsChanged {},
    KeyboardLayoutsChanged {},
    KeyboardLayoutSwitched {},
    OverviewOpenedOrClosed {},
    ConfigLoaded {},
    ScreenshotCaptured {},
    CastsChanged {},
    CastStartedOrChanged {},
    CastStopped {},
}

pub(super) fn socket_path() -> Result<PathBuf> {
    let path =
        PathBuf::from(std::env::var_os("NIRI_SOCKET").context("Niri 未提供来源事件 socket")?);
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("未设置 XDG_RUNTIME_DIR")?);
    let display =
        PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").context("未设置 WAYLAND_DISPLAY")?);
    let display = display
        .file_name()
        .and_then(|name| name.to_str())
        .context("Wayland display 名称无效")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Niri socket 名称无效")?;
    if path.parent() != Some(runtime.as_path())
        || !name.starts_with(&format!("niri.{display}."))
        || !name.ends_with(".sock")
    {
        bail!("Niri 来源 socket 不属于当前用户的 Wayland 会话");
    }
    Ok(path)
}

/// 单个事件驱动连接；关闭或失效后明确失败，不轮询、不重连到其他桌面。
pub(super) async fn watch(
    endpoint: Endpoint,
    mut changed: impl FnMut(Update) -> bool,
) -> std::result::Result<(), &'static str> {
    let mut reader = tokio::time::timeout(Duration::from_secs(1), async {
        let mut stream = endpoint.connect().await?;
        stream
            .write_all(b"\"EventStream\"\n")
            .await
            .map_err(|_| "无法订阅 Niri 来源事件")?;
        let mut reader = BufReader::new(stream);
        let response = message(&mut reader)
            .await
            .map_err(|_| "Niri 来源服务握手失败")?;
        let reply: serde_json::Value =
            serde_json::from_slice(&response).map_err(|_| "Niri 来源服务握手无效")?;
        if reply != serde_json::json!({"Ok": "Handled"}) {
            return Err("Niri 拒绝来源事件订阅");
        }
        Ok(reader)
    })
    .await
    .map_err(|_| "Niri 来源服务握手超时")??;

    let startup_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut focus = Focus::default();
    loop {
        let bytes = if focus.known {
            message(&mut reader).await
        } else {
            tokio::time::timeout_at(startup_deadline, message(&mut reader))
                .await
                .map_err(|_| "Niri 初始来源事件超时")?
        }
        .map_err(|_| "Niri 来源事件已关闭或超出大小预算")?;
        let event = serde_json::from_slice(&bytes).map_err(|_| "Niri 来源事件无效")?;
        let window = match event {
            Event::WindowsChanged { windows } => {
                let mut focused = windows.into_iter().filter(|window| window.is_focused);
                let window = focused.next().map(|window| u128::from(window.id));
                if focused.next().is_some() {
                    return Err("Niri 来源事件包含多个活动窗口");
                }
                window
            }
            Event::WindowFocusChanged { id } => id.map(u128::from),
            Event::WindowOpenedOrChanged { window } => {
                if window.is_focused {
                    Some(u128::from(window.id))
                } else if focus.window == Some(u128::from(window.id)) {
                    None
                } else {
                    continue;
                }
            }
            Event::WindowClosed { id } => {
                let id = u128::from(id);
                if focus.window == Some(id) && focus.change(None) && !changed(Update::Changed(None))
                {
                    return Ok(());
                }
                if !changed(Update::Closed(id)) {
                    return Ok(());
                }
                continue;
            }
            _ => continue,
        };
        if focus.change(window) && !changed(Update::Changed(window)) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::SocketPath;
    use super::*;

    #[tokio::test]
    async fn focus_transport_subscribes_once_and_keeps_aba_changes() {
        let (path, listener) = SocketPath::bind();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            assert_eq!(message(&mut reader).await.unwrap(), b"\"EventStream\"\n");
            reader.get_mut().write_all(concat!(
                "{\"Ok\":\"Handled\"}\n",
                "{\"WindowsChanged\":{\"windows\":[{\"id\":1,\"is_focused\":true}]}}\n",
                "{\"WindowOpenedOrChanged\":{\"window\":{\"id\":3,\"is_focused\":false}}}\n",
                "{\"WindowClosed\":{\"id\":3}}\n",
                "{\"WindowOpenedOrChanged\":{\"window\":{\"id\":1,\"is_focused\":false}}}\n",
                "{\"WindowFocusChanged\":{\"id\":2}}\n",
                "{\"WindowFocusChanged\":{\"id\":1}}\n",
                "{\"WindowClosed\":{\"id\":1}}\n",
            ).as_bytes()).await.unwrap();
        });
        let mut events = Vec::new();
        watch(path.endpoint(), |update| {
            events.push(update);
            update != Update::Closed(1)
        })
        .await
        .unwrap();
        assert_eq!(
            events,
            [
                Update::Changed(Some(1)),
                Update::Closed(3),
                Update::Changed(None),
                Update::Changed(Some(2)),
                Update::Changed(Some(1)),
                Update::Changed(None),
                Update::Closed(1),
            ]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_and_inconsistent_focus_fail_without_reconnect() {
        for (response, expected) in [
            (
                "{\"Ok\":\"Handled\"}\n",
                "Niri 来源事件已关闭或超出大小预算",
            ),
            (
                "{\"Ok\":\"Handled\"}\n{\"WindowsChanged\":{\"windows\":[{\"id\":1,\"is_focused\":true},{\"id\":2,\"is_focused\":true}]}}\n",
                "Niri 来源事件包含多个活动窗口",
            ),
            (
                "{\"Err\":\"private server detail\"}\n",
                "Niri 拒绝来源事件订阅",
            ),
        ] {
            let (path, listener) = SocketPath::bind();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                message(&mut reader).await.unwrap();
                reader
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            });
            assert_eq!(watch(path.endpoint(), |_| true).await, Err(expected));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelling_a_focus_subscription_closes_the_connection() {
        use tokio::io::AsyncReadExt;
        let (path, listener) = SocketPath::bind();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            message(&mut reader).await.unwrap();
            reader
                .get_mut()
                .write_all(b"{\"Ok\":\"Handled\"}\n{\"WindowFocusChanged\":{\"id\":1}}\n")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(reader.read(&mut byte).await.unwrap(), 0);
        });
        let (ready, receive) = tokio::sync::oneshot::channel();
        let mut ready = Some(ready);
        let task = tokio::spawn(watch(path.endpoint(), move |_| {
            ready.take().unwrap().send(()).unwrap();
            true
        }));
        receive.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn non_focus_events_and_window_titles_are_not_retained() {
        assert!(matches!(
            serde_json::from_str::<Event>(r#"{"WindowLayoutsChanged":{"changes":[]}}"#).unwrap(),
            Event::WindowLayoutsChanged {}
        ));
        assert!(serde_json::from_str::<Event>(r#"{"UnrecognizedEvent":{}}"#).is_err());
        let Event::WindowOpenedOrChanged { window } = serde_json::from_str(r#"{"WindowOpenedOrChanged":{"window":{"id":4,"is_focused":true,"title":"private title","app_id":"sample"}}}"#).unwrap() else {
            panic!("expected a window identity");
        };
        assert_eq!(window.id, 4);
        assert!(window.is_focused);
    }
}
