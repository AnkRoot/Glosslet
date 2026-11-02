//! 当前登录会话的控制入口。只传意图，不传选区；限长、限时并确认送达。

use crate::core::{
    Command, ControlFuture, ControlLease, ControlRequest, ControlServer, ControlSink,
};
use anyhow::{Context, Result, bail};
use interprocess::local_socket::{
    ListenerOptions, Name,
    tokio::{Listener, Stream, prelude::*},
};
use std::{
    fs::{File, OpenOptions},
    hash::{DefaultHasher, Hash, Hasher},
    io,
    path::PathBuf,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Semaphore, oneshot},
};

const MAX_BYTES: usize = 256;
const MAX_REPLY_BYTES: usize = 4096;
const TIMEOUT: Duration = Duration::from_millis(800);

pub(super) struct Owner {
    path: PathBuf,
    _lock: File,
}

impl Owner {
    pub fn claim() -> Result<Option<Self>> {
        let path = path()?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        }
        let lock = options.open(path.with_extension("lock"))?;
        match lock.try_lock() {
            Ok(()) => Ok(Some(Self { path, _lock: lock })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// 在应用运行时的 enter guard 内绑定；只有持锁者可以回收残留 socket。
    pub fn bind(self) -> Result<Server> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            match std::fs::symlink_metadata(&self.path) {
                Ok(meta)
                    if meta.file_type().is_socket()
                        && meta.uid() == rustix::process::geteuid().as_raw() =>
                {
                    std::fs::remove_file(&self.path)?;
                }
                Ok(_) => bail!("控制路径被非本用户 socket 占用"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let options = ListenerOptions::new().name(name(&self.path)?);
        #[cfg(windows)]
        let options = {
            use interprocess::os::windows::{
                local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor,
            };
            let (sid, _) = windows_identity()?;
            let sddl = widestring::U16CString::from_str(format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"))?;
            options.security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
        };
        let listener = options.create_tokio()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Server {
            listener,
            _owner: self,
        })
    }
}

pub(super) struct Server {
    listener: Listener,
    _owner: Owner,
}

impl Server {
    pub async fn serve(
        self,
        send: impl Fn(ControlRequest) + Send + Sync + 'static,
    ) -> io::Result<()> {
        let send = std::sync::Arc::new(send);
        let slots = std::sync::Arc::new(Semaphore::new(16));
        loop {
            let stream = self.listener.accept().await?;
            #[cfg(unix)]
            if authenticate(&stream).is_err() {
                continue;
            }
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let send = send.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = tokio::time::timeout(TIMEOUT, async {
                    let bytes = frame(&stream, MAX_BYTES).await?;
                    let command = serde_json::from_slice(&bytes)
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                    let (reply, received) = oneshot::channel();
                    send(ControlRequest { command, reply });
                    let result = received.await.map_err(|_| io::ErrorKind::BrokenPipe)?;
                    let mut response = serde_json::to_vec(&result)?;
                    if response.len() >= MAX_REPLY_BYTES {
                        response = serde_json::to_vec(&Err::<String, _>("控制回复超过 4096 字节"))?;
                    }
                    response.push(b'\n');
                    (&stream).write_all(&response).await
                })
                .await;
            });
        }
    }
}

#[cfg(unix)]
fn authenticate(stream: &Stream) -> io::Result<()> {
    if stream.peer_creds()?.euid() != Some(rustix::process::geteuid().as_raw()) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

async fn frame(stream: &Stream, max_bytes: usize) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut reader = BufReader::new(stream).take(max_bytes as u64 + 1);
    let mut bytes = Vec::new();
    reader.read_until(b'\n', &mut bytes).await?;
    if bytes.len() > max_bytes || bytes.last() != Some(&b'\n') {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

pub(super) fn send(command: Command) -> Result<()> {
    let path = path()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let meta = std::fs::symlink_metadata(&path)
            .context("未找到当前会话的 Glosslet；请先启动常驻实例")?;
        if !meta.file_type().is_socket()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o077 != 0
        {
            bail!("控制 socket 的类型、用户或权限无效");
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::time::timeout(TIMEOUT, async {
            let stream = Stream::connect(name(&path)?)
                .await
                .context("无法联系当前会话中的 Glosslet；常驻实例未运行或 socket 已失效")?;
            #[cfg(unix)]
            authenticate(&stream)?;
            let mut bytes = serde_json::to_vec(&command)?;
            bytes.push(b'\n');
            (&stream).write_all(&bytes).await?;
            let reply = frame(&stream, MAX_REPLY_BYTES).await?;
            let result: std::result::Result<String, String> = serde_json::from_slice(&reply)?;
            result.map_err(anyhow::Error::msg)
        })
        .await
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    let message = result.context("Glosslet 控制请求超时，未确认送达")??;
    println!("{message}");
    Ok(())
}

fn name(path: &std::path::Path) -> io::Result<Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        path.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        path.file_name()
            .ok_or(io::ErrorKind::InvalidInput)?
            .to_ns_name::<GenericNamespaced>()
    }
}

fn path() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let (runtime, session) = {
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("未设置 XDG_RUNTIME_DIR")?);
        let kind = std::env::var("XDG_SESSION_TYPE").context("未设置 XDG_SESSION_TYPE")?;
        let session = match kind.as_str() {
            "wayland" => format!(
                "wayland:{}",
                std::env::var("WAYLAND_DISPLAY").context("未设置 WAYLAND_DISPLAY")?
            ),
            "x11" => format!(
                "x11:{}",
                std::env::var("DISPLAY").context("未设置 DISPLAY")?
            ),
            _ => bail!("当前会话不是明确的 Wayland 或 X11 会话"),
        };
        (runtime, session)
    };
    #[cfg(target_os = "macos")]
    let (runtime, session) = {
        let home = PathBuf::from(std::env::var_os("HOME").context("未设置 HOME")?);
        let runtime = home.join("Library/Caches/Glosslet/runtime");
        create_private_directory(&runtime)?;
        (runtime, "macos".to_owned())
    };
    #[cfg(windows)]
    let (runtime, session) = {
        let runtime =
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("未设置 LOCALAPPDATA")?)
                .join("Glosslet/runtime");
        std::fs::create_dir_all(&runtime)?;
        let (sid, session) = windows_identity()?;
        (runtime, format!("{sid}-{session}"))
    };
    let meta = std::fs::symlink_metadata(&runtime)?;
    if !runtime.is_absolute() || !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("运行目录必须是普通目录")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            bail!("运行目录必须属于当前用户且其他用户不可访问");
        }
    }
    let mut hash = DefaultHasher::new();
    session.hash(&mut hash);
    Ok(runtime.join(format!(
        "{}-{:016x}.sock",
        env!("CARGO_PKG_NAME"),
        hash.finish()
    )))
}

#[cfg(target_os = "macos")]
fn create_private_directory(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}

#[cfg(windows)]
fn windows_identity() -> Result<(String, u32)> {
    use windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
            Security::{
                Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY,
                TOKEN_USER, TokenUser,
            },
            System::{
                RemoteDesktop::ProcessIdToSessionId,
                Threading::{GetCurrentProcess, OpenProcessToken},
            },
        },
        core::PWSTR,
    };
    // SAFETY: 所有缓冲区保持存活且对齐；系统分配的 SID 字符串及 token 均在返回前释放。
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let result = (|| {
            let mut size = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
            if size == 0 {
                bail!("无法读取当前登录用户 SID")
            }
            let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            GetTokenInformation(
                token,
                TokenUser,
                Some(data.as_mut_ptr().cast()),
                size,
                &mut size,
            )?;
            let user = &*data.as_ptr().cast::<TOKEN_USER>();
            let mut sid = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
            let text = sid.to_string();
            let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
            let mut session = 0;
            ProcessIdToSessionId(std::process::id(), &mut session)?;
            Ok((text?, session))
        })();
        let _ = CloseHandle(token);
        result
    }
}

impl ControlLease for Owner {
    fn bind(self: Box<Self>) -> Result<Box<dyn ControlServer>> {
        Ok(Box::new(Owner::bind(*self)?))
    }
}

impl ControlServer for Server {
    fn serve(self: Box<Self>, send: ControlSink) -> ControlFuture {
        Box::pin(async move { Server::serve(*self, move |request| send(request)).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::AsyncWriteExt;

    static ENDPOINT_ID: AtomicU64 = AtomicU64::new(0);

    struct Endpoint {
        directory: PathBuf,
        path: PathBuf,
        listener: Option<Listener>,
    }
    impl Endpoint {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let id = ENDPOINT_ID.fetch_add(1, Ordering::Relaxed);
            let directory =
                std::env::temp_dir().join(format!("glosslet-{}-{nonce}-{id}", std::process::id()));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join(format!("ipc-{}-{id}", std::process::id()));
            let listener = ListenerOptions::new()
                .name(name(&path).unwrap())
                .create_tokio()
                .unwrap();
            Self {
                directory,
                path,
                listener: Some(listener),
            }
        }
        async fn pair(&self) -> (Stream, Stream) {
            let (accepted, connected) = tokio::join!(
                self.listener.as_ref().unwrap().accept(),
                Stream::connect(name(&self.path).unwrap())
            );
            (accepted.unwrap(), connected.unwrap())
        }
    }
    impl Drop for Endpoint {
        fn drop(&mut self) {
            self.listener.take();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[tokio::test]
    async fn concurrent_test_endpoints_keep_their_own_frames() {
        let first = Endpoint::new();
        let second = Endpoint::new();
        assert_ne!(first.path.file_name(), second.path.file_name());
        tokio::time::timeout(TIMEOUT, async {
            let (first_server, mut first_client) = first.pair().await;
            let (second_server, mut second_client) = second.pair().await;
            first_client.write_all(b"first\n").await.unwrap();
            second_client.write_all(b"second\n").await.unwrap();
            assert_eq!(frame(&first_server, MAX_BYTES).await.unwrap(), b"first\n");
            assert_eq!(frame(&second_server, MAX_BYTES).await.unwrap(), b"second\n");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn frames_require_a_complete_bounded_line_and_same_user() {
        let endpoint = Endpoint::new();
        let (server, mut client) = endpoint.pair().await;
        #[cfg(unix)]
        authenticate(&server).unwrap();
        client.write_all(&vec![b'x'; MAX_BYTES - 1]).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        assert_eq!(frame(&server, MAX_BYTES).await.unwrap().len(), MAX_BYTES);

        let (server, mut client) = endpoint.pair().await;
        client.write_all(&vec![b'x'; MAX_BYTES]).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        assert_eq!(
            frame(&server, MAX_BYTES).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let (server, mut client) = endpoint.pair().await;
        client.write_all(b"{\"command\":\"lookup\"}").await.unwrap();
        drop(client);
        assert_eq!(
            frame(&server, MAX_BYTES).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let (server, mut client) = endpoint.pair().await;
        client.write_all(b"partial").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), frame(&server, MAX_BYTES))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn response_frames_are_bounded_without_expanding_command_frames() {
        let endpoint = Endpoint::new();
        let (server, mut client) = endpoint.pair().await;
        client
            .write_all(&vec![b'x'; MAX_REPLY_BYTES - 1])
            .await
            .unwrap();
        client.write_all(b"\n").await.unwrap();
        assert_eq!(
            frame(&server, MAX_REPLY_BYTES).await.unwrap().len(),
            MAX_REPLY_BYTES
        );
        let (server, mut client) = endpoint.pair().await;
        client
            .write_all(&vec![b'x'; MAX_REPLY_BYTES])
            .await
            .unwrap();
        client.write_all(b"\n").await.unwrap();
        assert_eq!(
            frame(&server, MAX_REPLY_BYTES).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn control_ack_is_application_reply_not_just_a_successful_write() {
        let mut endpoint = Endpoint::new();
        let path = endpoint.path.clone();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(endpoint.directory.join("lock"))
            .unwrap();
        let server = Server {
            listener: endpoint.listener.take().unwrap(),
            _owner: Owner {
                path: path.clone(),
                _lock: lock,
            },
        };
        let (send, mut messages) = tokio::sync::mpsc::unbounded_channel();
        let serving = tokio::spawn(server.serve(move |message| {
            send.send(message).unwrap();
        }));
        let mut client = Stream::connect(name(&path).unwrap()).await.unwrap();
        client
            .write_all(b"{\"command\":\"status\"}\n")
            .await
            .unwrap();
        let message = messages.recv().await.unwrap();
        assert_eq!(message.command, Command::Status);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), frame(&client, MAX_REPLY_BYTES))
                .await
                .is_err()
        );
        message.reply.send(Ok("ready".into())).unwrap();
        let response: std::result::Result<String, String> =
            serde_json::from_slice(&frame(&client, MAX_REPLY_BYTES).await.unwrap()).unwrap();
        assert_eq!(response.unwrap(), "ready");
        for result in [
            Ok(serde_json::to_string_pretty(&crate::core::Settings::default()).unwrap()),
            Err("设置保存失败；运行值未改变".repeat(20)),
            Ok("x".repeat(MAX_REPLY_BYTES)),
        ] {
            let client = Stream::connect(name(&path).unwrap()).await.unwrap();
            (&client)
                .write_all(b"{\"command\":\"settings\"}\n")
                .await
                .unwrap();
            let message = messages.recv().await.unwrap();
            assert_eq!(message.command, Command::Settings);
            let expected = if serde_json::to_vec(&result).unwrap().len() >= MAX_REPLY_BYTES {
                Err("控制回复超过 4096 字节".to_owned())
            } else {
                result.clone()
            };
            message.reply.send(result).unwrap();
            let response: std::result::Result<String, String> =
                serde_json::from_slice(&frame(&client, MAX_REPLY_BYTES).await.unwrap()).unwrap();
            assert_eq!(response, expected);
        }
        let accepted = Stream::connect(name(&path).unwrap()).await.unwrap();
        (&accepted)
            .write_all(b"{\"command\":\"lookup\",\"text\":\"forbidden\"}\n")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), messages.recv())
                .await
                .is_err()
        );
        serving.abort();
        let _ = serving.await;
    }
}
