//! Sway 原生 IPC：订阅窗口/关闭事件，不读取 I3SOCK，不轮询树或输入设备。

use std::time::Duration;

use serde::{Deserialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use super::{Endpoint, Focus, MAX_MESSAGE_BYTES, Update};

const MAGIC: &[u8; 6] = b"i3-ipc"; // Sway 当前协议的固定 magic，不是 X11/i3 回退。
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;
const GET_SEATS: u32 = 101;
const WINDOW_EVENT: u32 = 0x8000_0003;
const SHUTDOWN_EVENT: u32 = 0x8000_0006;

async fn send(
    stream: &mut UnixStream,
    kind: u32,
    payload: &[u8],
) -> std::result::Result<(), &'static str> {
    let mut header = Vec::from(MAGIC.as_slice());
    header.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    header.extend_from_slice(&kind.to_ne_bytes());
    stream
        .write_all(&header)
        .await
        .map_err(|_| "Sway IPC 请求失败")?;
    stream
        .write_all(payload)
        .await
        .map_err(|_| "Sway IPC 请求失败")
}

async fn frame(stream: &mut UnixStream) -> std::result::Result<(u32, Vec<u8>), &'static str> {
    let mut header = [0; 14];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|_| "Sway IPC 连接已关闭")?;
    if &header[..6] != MAGIC {
        return Err("Sway IPC magic 无效");
    }
    let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
    let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
    if len > MAX_MESSAGE_BYTES {
        return Err("Sway IPC 响应超出大小预算");
    }
    let mut bytes = vec![0; len];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|_| "Sway IPC 响应不完整")?;
    Ok((kind, bytes))
}

async fn query<T: DeserializeOwned>(
    endpoint: &Endpoint,
    kind: u32,
) -> std::result::Result<T, &'static str> {
    let mut stream = endpoint.connect().await?;
    send(&mut stream, kind, b"").await?;
    let (received, bytes) = frame(&mut stream).await?;
    if received != kind {
        return Err("Sway IPC 响应类型不匹配");
    }
    serde_json::from_slice(&bytes).map_err(|_| "Sway IPC 响应无效")
}

#[derive(Deserialize)]
struct Seat {
    focus: u64,
}

#[derive(Deserialize)]
struct Node {
    id: u64,
    shell: Option<String>,
    nodes: Vec<Node>,
    floating_nodes: Vec<Node>,
}

impl Node {
    fn window(&self, id: u64) -> bool {
        (self.id == id && self.shell.is_some())
            || self
                .nodes
                .iter()
                .chain(&self.floating_nodes)
                .any(|node| node.window(id))
    }
}

#[derive(Deserialize)]
struct Success {
    success: bool,
}

#[derive(Deserialize)]
struct WindowEvent {
    change: String,
    container: Node,
}

pub(super) async fn watch(
    endpoint: Endpoint,
    mut changed: impl FnMut(Update) -> bool,
) -> std::result::Result<(), &'static str> {
    let (mut stream, window) = tokio::time::timeout(Duration::from_secs(1), async {
        let mut stream = endpoint.connect().await?;
        send(&mut stream, SUBSCRIBE, br#"["window","shutdown"]"#).await?;
        let (kind, bytes) = frame(&mut stream).await?;
        let result: Success = serde_json::from_slice(&bytes).map_err(|_| "Sway 订阅响应无效")?;
        if kind != SUBSCRIBE || !result.success {
            return Err("Sway 拒绝来源事件订阅");
        }
        let seats: Vec<Seat> = query(&endpoint, GET_SEATS).await?;
        if seats.len() != 1 {
            return Err("Sway 来源座位不唯一，拒绝混用选区");
        }
        let focused = seats[0].focus;
        let window = if focused == 0 {
            None
        } else {
            let tree: Node = query(&endpoint, GET_TREE).await?;
            tree.window(focused).then_some(u128::from(focused))
        };
        Ok((stream, window))
    })
    .await
    .map_err(|_| "Sway 来源初始化超时")??;
    let mut focus = Focus::default();
    focus.change(window);
    if !changed(Update::Changed(window)) {
        return Ok(());
    }
    loop {
        let (kind, bytes) = frame(&mut stream).await?;
        match kind {
            SHUTDOWN_EVENT => return Err("Sway 来源服务已关闭"),
            WINDOW_EVENT => {
                let event: WindowEvent =
                    serde_json::from_slice(&bytes).map_err(|_| "Sway 窗口事件无效")?;
                match event.change.as_str() {
                    "focus" => {
                        let window = event
                            .container
                            .shell
                            .is_some()
                            .then_some(u128::from(event.container.id));
                        if focus.change(window) && !changed(Update::Changed(window)) {
                            return Ok(());
                        }
                    }
                    "close" => {
                        let id = u128::from(event.container.id);
                        if focus.window == Some(id)
                            && focus.change(None)
                            && !changed(Update::Changed(None))
                        {
                            return Ok(());
                        }
                        if !changed(Update::Closed(id)) {
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
            _ => return Err("Sway 返回了未订阅的事件类型"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::SocketPath;
    use super::*;

    const TREE: &[u8] = br#"{"id":0,"nodes":[{"id":1,"shell":"xdg_shell","title":"private title","nodes":[],"floating_nodes":[]}],"floating_nodes":[]}"#;
    const FOCUS: &[u8] = br#"{"change":"focus","container":{"id":2,"shell":"xdg_shell","nodes":[],"floating_nodes":[]}}"#;
    const CLOSE: &[u8] = br#"{"change":"close","container":{"id":2,"shell":"xdg_shell","nodes":[],"floating_nodes":[]}}"#;

    #[test]
    fn focused_workspace_is_not_a_source_window() {
        let tree: Node = serde_json::from_slice(TREE).unwrap();
        assert!(!tree.window(0));
        assert!(tree.window(1));
        assert!(!tree.window(2));
    }

    #[tokio::test]
    async fn native_frames_validate_magic_type_length_and_completeness() {
        for (header, expected) in [
            (
                [
                    b"wrong!".as_slice(),
                    &0u32.to_ne_bytes(),
                    &0u32.to_ne_bytes(),
                ]
                .concat(),
                "Sway IPC magic 无效",
            ),
            (
                [
                    MAGIC.as_slice(),
                    &(MAX_MESSAGE_BYTES as u32 + 1).to_ne_bytes(),
                    &0u32.to_ne_bytes(),
                ]
                .concat(),
                "Sway IPC 响应超出大小预算",
            ),
            (
                [MAGIC.as_slice(), &2u32.to_ne_bytes(), &0u32.to_ne_bytes()].concat(),
                "Sway IPC 响应不完整",
            ),
        ] {
            let (path, listener) = SocketPath::bind();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.write_all(&header).await.unwrap();
            });
            let mut stream = path.endpoint().connect().await.unwrap();
            assert_eq!(frame(&mut stream).await, Err(expected));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn subscription_bootstrap_and_window_events_are_event_driven() {
        let (path, listener) = SocketPath::bind();
        let server = tokio::spawn(async move {
            let (mut events, _) = listener.accept().await.unwrap();
            assert_eq!(
                frame(&mut events).await.unwrap(),
                (SUBSCRIBE, br#"["window","shutdown"]"#.to_vec())
            );
            send(&mut events, SUBSCRIBE, br#"{"success":true}"#)
                .await
                .unwrap();
            let (mut seats, _) = listener.accept().await.unwrap();
            assert_eq!(frame(&mut seats).await.unwrap(), (GET_SEATS, vec![]));
            send(&mut seats, GET_SEATS, br#"[{"focus":1}]"#)
                .await
                .unwrap();
            let (mut tree, _) = listener.accept().await.unwrap();
            assert_eq!(frame(&mut tree).await.unwrap(), (GET_TREE, vec![]));
            send(&mut tree, GET_TREE, TREE).await.unwrap();
            send(&mut events, WINDOW_EVENT, FOCUS).await.unwrap();
            send(&mut events, WINDOW_EVENT, CLOSE).await.unwrap();
        });
        let mut updates = Vec::new();
        watch(path.endpoint(), |update| {
            updates.push(update);
            update != Update::Closed(2)
        })
        .await
        .unwrap();
        assert_eq!(
            updates,
            [
                Update::Changed(Some(1)),
                Update::Changed(Some(2)),
                Update::Changed(None),
                Update::Closed(2)
            ]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_subscription_does_not_switch_to_another_backend() {
        let (path, listener) = SocketPath::bind();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            frame(&mut stream).await.unwrap();
            send(
                &mut stream,
                SUBSCRIBE,
                br#"{"success":false,"error":"private detail"}"#,
            )
            .await
            .unwrap();
        });
        assert_eq!(
            watch(path.endpoint(), |_| true).await,
            Err("Sway 拒绝来源事件订阅")
        );
        server.await.unwrap();
    }
}
