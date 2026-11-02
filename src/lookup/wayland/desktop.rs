//! 本功能的桌面契约与显式接入。焦点身份不代表 PRIMARY 所有者身份。

mod hyprland;
mod kde;
mod niri;
mod sway;

use std::{
    io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt},
    net::UnixStream,
};

pub(super) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Update {
    Changed(Option<u128>),
    Closed(u128),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub window: u128,
    epoch: u64,
}

#[derive(Default)]
pub(super) struct Focus {
    known: bool,
    window: Option<u128>,
    epoch: u64,
}

impl Focus {
    pub fn change(&mut self, window: Option<u128>) -> bool {
        if self.known && self.window == window {
            return false;
        }
        self.known = true;
        self.window = window;
        self.epoch += 1;
        true
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        Some(Snapshot {
            window: self.window?,
            epoch: self.epoch,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Niri,
    Hyprland,
    Sway,
    Kde,
}

impl Kind {
    fn identify(session: &str) -> Result<Self> {
        let mut recognized =
            session
                .split(':')
                .filter_map(|part| match part.to_ascii_lowercase().as_str() {
                    "niri" => Some(Self::Niri),
                    "hyprland" => Some(Self::Hyprland),
                    "sway" => Some(Self::Sway),
                    "kde" => Some(Self::Kde),
                    _ => None,
                });
        let kind = recognized
            .next()
            .context("此桌面尚未接入原生取词与窗口契约；不使用其他桌面或 X11 回退")?;
        if recognized.any(|other| other != kind) {
            bail!("桌面身份不唯一，拒绝混用不同 compositor 的上下文");
        }
        Ok(kind)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Niri => "niri",
            Self::Hyprland => "hyprland",
            Self::Sway => "sway",
            Self::Kde => "KDE",
        }
    }
}

#[derive(Clone)]
struct Endpoint {
    path: PathBuf,
    pid: i32,
}

impl Endpoint {
    fn validate(path: PathBuf, pid: i32) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(&path).context("桌面 IPC socket 不可达")?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            bail!("桌面 IPC socket 必须是当前用户拥有的 socket，不接受符号链接");
        }
        Ok(Self { path, pid })
    }

    async fn connect(&self) -> std::result::Result<UnixStream, &'static str> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.path))
            .await
            .map_err(|_| "桌面 IPC 连接超时")?
            .map_err(|_| "桌面 IPC 连接失败")?;
        let peer = stream.peer_cred().map_err(|_| "无法确认桌面 IPC 身份")?;
        if peer.uid() != rustix::process::geteuid().as_raw() || peer.pid() != Some(self.pid) {
            return Err("桌面 IPC 与当前 Wayland compositor 不是同一用户或进程");
        }
        Ok(stream)
    }
}

pub(super) struct NativeTrigger {
    pub window: Option<u128>,
    pub point: (f64, f64),
}

#[derive(Clone)]
pub(super) struct Broker(Backend);
#[derive(Clone)]
enum Backend {
    Socket(Kind, Endpoint),
    Kde(kde::Broker),
}

impl Broker {
    pub fn detect(pid: i32) -> Result<Self> {
        let session = std::env::var("XDG_CURRENT_DESKTOP")
            .context("未设置 XDG_CURRENT_DESKTOP，无法确认桌面契约")?;
        let kind = Kind::identify(&session)?;
        if kind == Kind::Kde {
            return Ok(Self(Backend::Kde(kde::Broker::new(pid))));
        }
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("未设置 XDG_RUNTIME_DIR")?);
        let path = match kind {
            Kind::Niri => niri::socket_path()?,
            Kind::Hyprland => {
                let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
                    .context("未设置 HYPRLAND_INSTANCE_SIGNATURE")?;
                if !single_component(&signature) {
                    bail!("Hyprland 实例签名无效")
                }
                runtime.join("hypr").join(signature).join(".socket.sock")
            }
            Kind::Sway => {
                let path = PathBuf::from(
                    std::env::var_os("SWAYSOCK").context("未设置 SWAYSOCK；不读取 I3SOCK")?,
                );
                if path.parent() != Some(runtime.as_path()) {
                    bail!("Sway socket 不属于当前运行时目录")
                }
                path
            }
            Kind::Kde => unreachable!(),
        };
        let endpoint = Endpoint::validate(path, pid)?;
        if kind == Kind::Hyprland {
            Endpoint::validate(endpoint.path.with_file_name(".socket2.sock"), pid)?;
        }
        Ok(Self(Backend::Socket(kind, endpoint)))
    }
    pub fn name(&self) -> &'static str {
        match &self.0 {
            Backend::Socket(kind, _) => kind.name(),
            Backend::Kde(_) => Kind::Kde.name(),
        }
    }
    pub fn native_anchor(&self) -> bool {
        matches!(
            &self.0,
            Backend::Kde(_) | Backend::Socket(Kind::Hyprland, _)
        )
    }
    pub fn native_trigger(&self) -> bool {
        matches!(&self.0, Backend::Kde(_))
    }
    pub async fn check_plugin(&self) -> Result<()> {
        if let Backend::Kde(kde) = &self.0 {
            kde.check().await?;
        }
        Ok(())
    }
    pub async fn invoke_lookup(&self) -> std::result::Result<(), &'static str> {
        match &self.0 {
            Backend::Kde(kde) => kde.invoke_lookup().await,
            _ => Err("此桌面不使用插件触发入口"),
        }
    }
    pub async fn watch(
        &self,
        changed: impl FnMut(Update) -> bool,
        lookup: impl FnMut(NativeTrigger) -> bool,
    ) -> std::result::Result<(), &'static str> {
        match &self.0 {
            Backend::Kde(kde) => kde.watch(changed, lookup).await,
            Backend::Socket(Kind::Niri, endpoint) => niri::watch(endpoint.clone(), changed).await,
            Backend::Socket(Kind::Sway, endpoint) => sway::watch(endpoint.clone(), changed).await,
            Backend::Socket(Kind::Hyprland, endpoint) => {
                hyprland::watch(
                    endpoint.clone(),
                    Endpoint {
                        path: endpoint.path.with_file_name(".socket2.sock"),
                        pid: endpoint.pid,
                    },
                    changed,
                )
                .await
            }
            Backend::Socket(Kind::Kde, _) => unreachable!(),
        }
    }
    pub async fn anchor(
        &self,
        window: Option<u128>,
    ) -> std::result::Result<(f64, f64), &'static str> {
        match &self.0 {
            Backend::Kde(kde) => kde.anchor(window).await,
            Backend::Socket(Kind::Hyprland, endpoint) => hyprland::anchor(endpoint, window).await,
            _ => Err("此桌面未提供原生鼠标锚点接口"),
        }
    }
}

fn single_component(value: &str) -> bool {
    let mut components = Path::new(value).components();
    !value.is_empty()
        && value.len() <= 128
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

async fn message<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let len = end.map_or(available.len(), |index| index + 1);
        if len > MAX_MESSAGE_BYTES.saturating_sub(bytes.len()) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        bytes.extend_from_slice(&available[..len]);
        reader.consume(len);
        if end.is_some() {
            return Ok(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::BufReader, net::UnixListener};

    pub(super) struct SocketPath(pub PathBuf);

    impl SocketPath {
        pub fn bind() -> (Self, UnixListener) {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = Self(std::env::temp_dir().join(format!(
                "glosslet-ipc-{}-{stamp}-{sequence}.sock",
                std::process::id()
            )));
            let listener = UnixListener::bind(&path.0).unwrap();
            (path, listener)
        }

        pub fn endpoint(&self) -> Endpoint {
            Endpoint {
                path: self.0.clone(),
                pid: std::process::id() as i32,
            }
        }
    }

    impl Drop for SocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn desktop_selection_never_uses_another_backend_as_a_fallback() {
        assert_eq!(Kind::identify("Hyprland").unwrap(), Kind::Hyprland);
        assert_eq!(Kind::identify("niri:Shell").unwrap(), Kind::Niri);
        assert_eq!(Kind::identify("sway").unwrap(), Kind::Sway);
        assert!(Kind::identify("GNOME").is_err());
        assert_eq!(Kind::identify("KDE").unwrap(), Kind::Kde);
        assert!(Kind::identify("i3").is_err());
        assert!(Kind::identify("niri:Hyprland").is_err());
        assert!(Kind::identify("").is_err());
    }

    #[test]
    fn returning_to_a_window_does_not_restore_an_old_snapshot() {
        let mut focus = Focus::default();
        assert!(focus.snapshot().is_none());
        focus.change(Some(1));
        let original = focus.snapshot().unwrap();
        assert!(!focus.change(Some(1)));
        focus.change(Some(2));
        focus.change(Some(1));
        assert_ne!(focus.snapshot(), Some(original));
        focus.change(None);
        assert!(focus.snapshot().is_none());
    }

    #[test]
    fn instance_signatures_cannot_escape_the_runtime_directory() {
        assert!(single_component("commit_123_456"));
        for value in ["", "..", "../socket", "/tmp/socket", "a/b"] {
            assert!(!single_component(value));
        }
    }

    #[tokio::test]
    async fn ipc_frames_are_bounded_and_must_be_complete() {
        let mut reader = BufReader::new(b"first\nsecond\n".as_slice());
        assert_eq!(message(&mut reader).await.unwrap(), b"first\n");
        assert_eq!(message(&mut reader).await.unwrap(), b"second\n");
        assert_eq!(
            message(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let bytes = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let mut reader = BufReader::new(bytes.as_slice());
        assert_eq!(
            message(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn socket_credentials_must_match_the_wayland_process() {
        let (path, _listener) = SocketPath::bind();
        let wrong = Endpoint {
            pid: std::process::id() as i32 + 1,
            ..path.endpoint()
        };
        assert!(matches!(
            wrong.connect().await,
            Err("桌面 IPC 与当前 Wayland compositor 不是同一用户或进程")
        ));
        assert!(path.endpoint().connect().await.is_ok());
    }
}
