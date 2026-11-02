//! Hyprland 的只读 IPC。只订阅窗口身份；鼠标锚点按明确触发查询，不抓取输入。

use std::time::Duration;

use serde::{Deserialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

use super::{Endpoint, Focus, MAX_MESSAGE_BYTES, Update, message};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(untagged)]
enum ActiveWindow {
    Window { address: String },
    Empty(Empty),
}

#[derive(Deserialize)]
struct Point {
    x: f64,
    y: f64,
}

fn address(value: &str) -> std::result::Result<Option<u128>, &'static str> {
    if value.is_empty() {
        return Ok(None);
    }
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.is_empty() || value.len() > 16 || !value.bytes().all(|ch| ch.is_ascii_hexdigit()) {
        return Err("Hyprland 窗口身份无效");
    }
    let window = u64::from_str_radix(value, 16).map_err(|_| "Hyprland 窗口身份无效")?;
    Ok((window != 0).then_some(u128::from(window)))
}

fn response<T: DeserializeOwned>(bytes: &[u8]) -> std::result::Result<T, &'static str> {
    // Serde 的 struct 也能接受数组；Hyprland 这两个接口只允许顶层对象。
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err("Hyprland IPC 响应不是对象");
    }
    serde_json::from_slice(bytes).map_err(|_| "Hyprland IPC 响应无效")
}

async fn query<T: DeserializeOwned>(
    endpoint: &Endpoint,
    command: &[u8],
) -> std::result::Result<T, &'static str> {
    let mut stream = endpoint.connect().await?;
    stream
        .write_all(command)
        .await
        .map_err(|_| "Hyprland IPC 请求失败")?;
    let mut bytes = Vec::new();
    stream
        .take(MAX_MESSAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "Hyprland IPC 响应已关闭")?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err("Hyprland IPC 响应超出大小预算");
    }
    response(&bytes)
}

async fn focused(endpoint: &Endpoint) -> std::result::Result<Option<u128>, &'static str> {
    match query(endpoint, b"j/activewindow").await? {
        ActiveWindow::Window { address: value } => address(&value),
        ActiveWindow::Empty(_) => Ok(None),
    }
}

pub(super) async fn anchor(
    endpoint: &Endpoint,
    expected: Option<u128>,
) -> std::result::Result<(f64, f64), &'static str> {
    tokio::time::timeout(Duration::from_millis(800), async {
        let before = focused(endpoint).await?;
        if expected.is_some() && before != expected {
            return Err("Hyprland 活动来源已变化，请重新触发");
        }
        let point: Point = query(endpoint, b"j/cursorpos").await?;
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err("Hyprland 鼠标锚点无效");
        }
        if focused(endpoint).await? != before {
            return Err("Hyprland 活动来源已变化，请重新触发");
        }
        Ok((point.x, point.y))
    })
    .await
    .map_err(|_| "Hyprland 鼠标锚点请求超时")?
}

fn event(bytes: &[u8]) -> std::result::Result<Option<Update>, &'static str> {
    let line = std::str::from_utf8(bytes).map_err(|_| "Hyprland 事件不是有效 UTF-8")?;
    let (kind, value) = line
        .trim_end_matches('\n')
        .split_once(">>")
        .ok_or("Hyprland 事件格式无效")?;
    Ok(match kind {
        "activewindowv2" => Some(Update::Changed(address(value)?)),
        "closewindow" => Some(Update::Closed(
            address(value)?.ok_or("Hyprland 关闭事件缺少窗口身份")?,
        )),
        "activelayer" => Some(Update::Changed(None)),
        _ => None,
    })
}

pub(super) async fn watch(
    command: Endpoint,
    events: Endpoint,
    mut changed: impl FnMut(Update) -> bool,
) -> std::result::Result<(), &'static str> {
    let (mut reader, window) = tokio::time::timeout(Duration::from_secs(1), async {
        let reader = BufReader::new(events.connect().await?);
        let window = focused(&command).await?;
        Ok::<_, &'static str>((reader, window))
    })
    .await
    .map_err(|_| "Hyprland 来源初始化超时")??;
    let mut focus = Focus::default();
    focus.change(window);
    if !changed(Update::Changed(window)) {
        return Ok(());
    }
    loop {
        let bytes = message(&mut reader)
            .await
            .map_err(|_| "Hyprland 来源事件已关闭或超出大小预算")?;
        match event(&bytes)? {
            Some(Update::Changed(window)) => {
                if focus.change(window) && !changed(Update::Changed(window)) {
                    return Ok(());
                }
            }
            Some(Update::Closed(window)) => {
                if focus.window == Some(window)
                    && focus.change(None)
                    && !changed(Update::Changed(None))
                {
                    return Ok(());
                }
                if !changed(Update::Closed(window)) {
                    return Ok(());
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::SocketPath;
    use super::*;
    use tokio::net::UnixListener;

    async fn replies(listener: UnixListener, messages: Vec<(&'static [u8], &'static [u8])>) {
        for (expected, response) in messages {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; expected.len()];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(request, expected);
            stream.write_all(response).await.unwrap();
        }
    }

    #[test]
    fn events_use_unique_addresses_and_never_window_titles() {
        assert_eq!(
            event(b"activewindowv2>>ab12\n").unwrap(),
            Some(Update::Changed(Some(0xab12)))
        );
        assert_eq!(
            event(b"closewindow>>ab12\n").unwrap(),
            Some(Update::Closed(0xab12))
        );
        assert_eq!(
            event(b"activewindowv2>>\n").unwrap(),
            Some(Update::Changed(None))
        );
        assert_eq!(event(b"activewindow>>class,private title\n").unwrap(), None);
        assert_eq!(
            event(b"activelayer>>glosslet\n").unwrap(),
            Some(Update::Changed(None))
        );
        for bytes in [
            b"activewindowv2>>not-an-address\n".as_slice(),
            b"closewindow>>\n",
            b"broken\n",
        ] {
            assert!(event(bytes).is_err());
        }
    }

    #[test]
    fn active_window_shape_is_not_silently_treated_as_no_focus() {
        assert!(matches!(
            serde_json::from_slice::<ActiveWindow>(b"{}").unwrap(),
            ActiveWindow::Empty(_)
        ));
        assert!(response::<ActiveWindow>(b"[]").is_err());
        assert!(response::<ActiveWindow>(br#"["0x1"]"#).is_err());
        assert!(response::<ActiveWindow>(br#"{"title":"private"}"#).is_err());
        for invalid in ["0x", "-1", "1,private", "12345678901234567"] {
            assert!(address(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn anchor_queries_only_metadata_and_checks_focus_on_both_sides() {
        let (path, listener) = SocketPath::bind();
        let server = tokio::spawn(replies(
            listener,
            vec![
                (
                    b"j/activewindow",
                    br#"{"address":"0xab12","title":"private title"}"#,
                ),
                (b"j/cursorpos", br#"{"x":-120.5,"y":900.25}"#),
                (b"j/activewindow", br#"{"address":"0xab12"}"#),
            ],
        ));
        assert_eq!(
            anchor(&path.endpoint(), Some(0xab12)).await.unwrap(),
            (-120.5, 900.25)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_source_change_rejects_anchor_without_an_input_grab_fallback() {
        let (path, listener) = SocketPath::bind();
        let server = tokio::spawn(replies(
            listener,
            vec![
                (b"j/activewindow", br#"{"address":"0xab12"}"#),
                (b"j/cursorpos", br#"{"x":10,"y":20}"#),
                (b"j/activewindow", br#"{"address":"0xcd34"}"#),
            ],
        ));
        assert_eq!(
            anchor(&path.endpoint(), Some(0xab12)).await,
            Err("Hyprland 活动来源已变化，请重新触发")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn event_stream_keeps_fast_window_changes_and_closures() {
        let (command, listener) = SocketPath::bind();
        let (events, event_listener) = SocketPath::bind();
        let commands = tokio::spawn(replies(
            listener,
            vec![(b"j/activewindow", br#"{"address":"0x1"}"#)],
        ));
        let server = tokio::spawn(async move {
            let (mut stream, _) = event_listener.accept().await.unwrap();
            stream
                .write_all(b"activewindowv2>>2\nactivewindowv2>>1\nclosewindow>>1\n")
                .await
                .unwrap();
        });
        let mut updates = Vec::new();
        watch(command.endpoint(), events.endpoint(), |update| {
            updates.push(update);
            update != Update::Closed(1)
        })
        .await
        .unwrap();
        assert_eq!(
            updates,
            [
                Update::Changed(Some(1)),
                Update::Changed(Some(2)),
                Update::Changed(Some(1)),
                Update::Changed(None),
                Update::Closed(1)
            ]
        );
        server.await.unwrap();
        commands.await.unwrap();
    }

    #[tokio::test]
    async fn responses_and_cancellation_have_resource_budgets() {
        let (path, listener) = SocketPath::bind();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; b"j/activewindow".len()];
            stream.read_exact(&mut request).await.unwrap();
            let _ = stream.write_all(&vec![b'x'; MAX_MESSAGE_BYTES + 1]).await;
        });
        assert_eq!(
            focused(&path.endpoint()).await,
            Err("Hyprland IPC 响应超出大小预算")
        );
        server.await.unwrap();
        let (path, listener) = SocketPath::bind();
        let (ready, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; b"j/activewindow".len()];
            stream.read_exact(&mut request).await.unwrap();
            ready.send(()).unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        let endpoint = path.endpoint();
        let task = tokio::spawn(async move { anchor(&endpoint, Some(1)).await });
        received.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }
}
