//! KWin 原生脚本只提供窗口身份与明确触发时的鼠标坐标；PRIMARY 仍由原生 Wayland 读取。

use super::{NativeTrigger, Update};
use anyhow::{Result, bail};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use zbus::{Connection, connection::Builder, message::Header};

const BUS: &str = "io.github.AnkRoot.Glosslet.KWin";
const PATH: &str = "/io/github/AnkRoot/Glosslet/KWin";

enum Event {
    Focus(Update),
    Lookup(NativeTrigger),
}
#[derive(Default)]
struct Context {
    generation: String,
    lease: u64,
    window: Option<u128>,
    epoch: u64,
    probe: Option<oneshot::Sender<(f64, f64)>>,
}

#[derive(Clone)]
pub(super) struct Broker {
    pid: u32,
    context: Arc<Mutex<Context>>,
}
impl Broker {
    pub fn new(pid: i32) -> Self {
        Self {
            pid: pid as u32,
            context: Arc::default(),
        }
    }
    pub async fn watch(
        &self,
        mut changed: impl FnMut(Update) -> bool,
        mut lookup: impl FnMut(NativeTrigger) -> bool,
    ) -> std::result::Result<(), &'static str> {
        verify(self.pid)
            .await
            .map_err(|_| "KWin D-Bus 身份或 Glosslet 脚本不可用")?;
        let (events, mut receiver) = mpsc::channel(32);
        let _conn = Builder::session()
            .map_err(|_| "用户总线不可用")?
            .name(BUS)
            .map_err(|_| "KWin 接入名称无效")?
            .serve_at(
                PATH,
                Port {
                    pid: self.pid,
                    context: self.context.clone(),
                    events,
                },
            )
            .map_err(|_| "无法装配 KWin 接入")?
            .build()
            .await
            .map_err(|_| "无法建立 KWin 接入；名称可能被占用")?;
        while let Some(event) = receiver.recv().await {
            let active = match event {
                Event::Focus(update) => changed(update),
                Event::Lookup(trigger) => lookup(trigger),
            };
            if !active {
                return Ok(());
            }
        }
        Err("KWin 脚本事件入口已关闭")
    }
    pub async fn invoke_lookup(&self) -> std::result::Result<(), &'static str> {
        invoke("Glosslet-Lookup")
            .await
            .map_err(|_| "无法调用 KWin 的 Glosslet-Lookup 动作；请启用原生脚本")
    }
    pub async fn anchor(
        &self,
        expected: Option<u128>,
    ) -> std::result::Result<(f64, f64), &'static str> {
        let (reply, receive) = oneshot::channel();
        let epoch = {
            let mut context = self.context.lock().map_err(|_| "KWin 上下文不可用")?;
            if expected.is_some() && expected != context.window {
                return Err("KWin 来源窗口已变化");
            }
            context.probe = Some(reply);
            (context.lease, context.epoch)
        };
        let point = tokio::time::timeout(std::time::Duration::from_millis(800), async {
            invoke("Glosslet-Probe")
                .await
                .map_err(|_| "KWin 公开定位动作不可用")?;
            receive.await.map_err(|_| "KWin 定位已被取消")
        })
        .await
        .map_err(|_| "KWin 定位超时")??;
        let context = self.context.lock().map_err(|_| "KWin 上下文不可用")?;
        if expected.is_some()
            && (context.window != expected || (context.lease, context.epoch) != epoch)
        {
            return Err("KWin 来源窗口已变化");
        }
        Ok(point)
    }
    pub async fn check(&self) -> Result<()> {
        verify(self.pid).await
    }
}

async fn verify(pid: u32) -> Result<()> {
    let conn = Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let owner = dbus.get_name_owner("org.kde.KWin".try_into()?).await?;
    if dbus.get_connection_unix_process_id(owner.into()).await? != pid {
        bail!("KWin 与 Wayland compositor 不是同一进程")
    }
    let proxy = zbus::Proxy::new(
        &conn,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await?;
    let loaded: bool = proxy.call("isScriptLoaded", &("glosslet",)).await?;
    if !loaded {
        bail!("Glosslet KWin 脚本未启用")
    }
    Ok(())
}
async fn invoke(action: &str) -> Result<()> {
    let conn = Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let owner = dbus
        .get_name_owner("org.kde.kglobalaccel".try_into()?)
        .await?;
    if dbus.get_connection_unix_user(owner.into()).await? != rustix::process::geteuid().as_raw() {
        bail!("快捷键服务不是当前用户")
    }
    let proxy = zbus::Proxy::new(
        &conn,
        "org.kde.kglobalaccel",
        "/component/kwin",
        "org.kde.kglobalaccel.Component",
    )
    .await?;
    proxy.call::<_, _, ()>("invokeShortcut", &(action,)).await?;
    Ok(())
}

struct Port {
    pid: u32,
    context: Arc<Mutex<Context>>,
    events: mpsc::Sender<Event>,
}
impl Port {
    async fn authenticate(&self, conn: &Connection, header: &Header<'_>) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("缺少调用者".into()))?;
        let dbus = zbus::fdo::DBusProxy::new(conn).await?;
        if dbus
            .get_connection_unix_process_id(sender.clone().into())
            .await?
            != self.pid
        {
            return Err(zbus::fdo::Error::AccessDenied(
                "调用者不是当前 KWin compositor".into(),
            ));
        }
        Ok(())
    }
    fn update(&self, lease: u64, window: &str, epoch: u64) -> zbus::fdo::Result<Option<u128>> {
        let window = window_id(window)?;
        let mut context = self
            .context
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("上下文不可用".into()))?;
        if lease != context.lease || epoch < context.epoch {
            return Err(zbus::fdo::Error::AccessDenied(
                "KWin 脚本租约或来源代次已失效".into(),
            ));
        }
        context.window = window;
        context.epoch = epoch;
        Ok(window)
    }
}
fn window_id(text: &str) -> zbus::fdo::Result<Option<u128>> {
    if text.is_empty() {
        return Ok(None);
    }
    uuid::Uuid::parse_str(text)
        .map(|id| Some(id.as_u128()))
        .map_err(|_| zbus::fdo::Error::InvalidArgs("KWin 窗口 UUID 无效".into()))
}
fn point(x: f64, y: f64) -> zbus::fdo::Result<(f64, f64)> {
    if !x.is_finite() || !y.is_finite() || x.abs() > i32::MAX as f64 || y.abs() > i32::MAX as f64 {
        return Err(zbus::fdo::Error::InvalidArgs("KWin 鼠标坐标无效".into()));
    }
    Ok((x, y))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    lease: u64,
    window: String,
    epoch: u64,
    point: Option<[f64; 2]>,
}
fn metadata(payload: &str) -> zbus::fdo::Result<Metadata> {
    if payload.len() > 1024 {
        return Err(zbus::fdo::Error::InvalidArgs("原生元数据过长".into()));
    }
    serde_json::from_str(payload)
        .map_err(|_| zbus::fdo::Error::InvalidArgs("原生元数据无效".into()))
}

#[zbus::interface(name = "io.github.AnkRoot.Glosslet.KWin1")]
impl Port {
    async fn attach(
        &self,
        generation: &str,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<String> {
        self.authenticate(conn, &header).await?;
        if generation.is_empty() || generation.len() > 64 {
            return Err(zbus::fdo::Error::InvalidArgs("脚本代次无效".into()));
        }
        let (lease, changed) = {
            let mut context = self
                .context
                .lock()
                .map_err(|_| zbus::fdo::Error::Failed("上下文不可用".into()))?;
            let changed = generation != context.generation;
            if changed {
                context.generation = generation.into();
                context.lease += 1;
                context.epoch = 0;
                context.window = None;
                context.probe = None;
            }
            (context.lease, changed)
        };
        if changed {
            let _ = self.events.send(Event::Focus(Update::Changed(None))).await;
        }
        Ok(lease.to_string())
    }
    async fn focused(
        &self,
        payload: &str,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        let data = metadata(payload)?;
        if data.point.is_some() {
            return Err(zbus::fdo::Error::InvalidArgs("焦点公告不得采样鼠标".into()));
        }
        let window = self.update(data.lease, &data.window, data.epoch)?;
        self.events
            .send(Event::Focus(Update::Changed(window)))
            .await
            .map_err(|_| zbus::fdo::Error::Failed("应用已退出".into()))
    }
    async fn lookup(
        &self,
        payload: &str,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        let data = metadata(payload)?;
        let window = self.update(data.lease, &data.window, data.epoch)?;
        let [x, y] = data
            .point
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("查词缺少鼠标锚点".into()))?;
        let point = point(x, y)?;
        self.events
            .send(Event::Lookup(NativeTrigger { window, point }))
            .await
            .map_err(|_| zbus::fdo::Error::Failed("应用已退出".into()))
    }
    async fn probed(
        &self,
        payload: &str,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        let data = metadata(payload)?;
        self.update(data.lease, &data.window, data.epoch)?;
        let [x, y] = data
            .point
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("定位缺少鼠标锚点".into()))?;
        let point = point(x, y)?;
        let mut context = self
            .context
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("上下文不可用".into()))?;
        if let Some(reply) = context.probe.take() {
            let _ = reply.send(point);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_metadata_preserves_full_uuid_and_rejects_invalid_geometry() {
        assert_eq!(
            window_id("{12345678-1234-5678-9abc-def012345678}").unwrap(),
            Some(0x12345678123456789abcdef012345678)
        );
        assert!(window_id("12").is_err());
        assert!(point(f64::NAN, 0.0).is_err());
        assert!(point(0.0, f64::INFINITY).is_err());
    }
}
