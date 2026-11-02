//! GNOME Wayland 使用显式安装的 Shell 扩展。用户总线只接受当前 compositor 进程。

use super::{controller::Controller, session::CaptureResult};
use crate::core::{
    Command, Content, ControlRequest, DisplayIdentity, Failure, Painter, Services, Ticket,
    Translation, Translator, View,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use zbus::{
    Connection, connection::Builder, message::Header, names::OwnedUniqueName,
    object_server::SignalEmitter,
};

const BUS: &str = "io.github.AnkRoot.Glosslet.Gnome";
const PATH: &str = "/io/github/AnkRoot/Glosslet/Gnome";
const UUID: &str = "glosslet@ankroot";

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    window: u64,
    epoch: u64,
    point: [f64; 2],
    bounds: [f64; 4],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    ticket: Ticket,
    origin: Origin,
    text: Option<String>,
    failure: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    identity: HostIdentity,
    kind: ActionKind,
    index: Option<usize>,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HostIdentity {
    scene: u64,
    display: DisplayIdentity,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Location {
    ticket: Ticket,
    origin: Origin,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Close,
    Pin,
    Theme,
    Retry,
    Term,
    Sentence,
}

enum Event {
    Control(ControlRequest),
    Attach(OwnedUniqueName, u64, u64),
    Detach,
    Focus(u64, u64),
    Captured(Capture),
    Located(Location),
    Painted(HostIdentity),
    Action(Action),
    Copied(HostIdentity, bool),
    Translated(Ticket, std::result::Result<Translation, Failure>),
}

struct Port {
    pid: u32,
    events: mpsc::Sender<Event>,
}
impl Port {
    async fn authenticate(
        &self,
        conn: &Connection,
        header: &Header<'_>,
    ) -> zbus::fdo::Result<OwnedUniqueName> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("缺少调用者身份".into()))?;
        let proxy = zbus::fdo::DBusProxy::new(conn).await?;
        let pid = proxy
            .get_connection_unix_process_id(sender.clone().into())
            .await?;
        if pid != self.pid {
            return Err(zbus::fdo::Error::AccessDenied(
                "调用者不是当前 GNOME compositor".into(),
            ));
        }
        Ok(sender.to_owned().into())
    }
    async fn send(&self, event: Event) -> zbus::fdo::Result<()> {
        self.events
            .send(event)
            .await
            .map_err(|_| zbus::fdo::Error::Failed("应用已退出".into()))
    }
}

#[zbus::interface(name = "io.github.AnkRoot.Glosslet.Gnome1")]
impl Port {
    async fn attach(
        &self,
        window: u64,
        epoch: u64,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let owner = self.authenticate(conn, &header).await?;
        self.send(Event::Attach(owner, window, epoch)).await
    }
    async fn detach(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        self.send(Event::Detach).await
    }
    async fn changed(
        &self,
        window: u64,
        epoch: u64,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        self.send(Event::Focus(window, epoch)).await
    }
    async fn captured(
        &self,
        payload: &str,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        if payload.len() > crate::core::MAX_SELECTION_BYTES * 6 + 4096 {
            return Err(zbus::fdo::Error::InvalidArgs("选区消息过长".into()));
        }
        let capture = serde_json::from_str(payload)
            .map_err(|_| zbus::fdo::Error::InvalidArgs("取词消息无效".into()))?;
        self.send(Event::Captured(capture)).await
    }
    async fn located(
        &self,
        payload: &str,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        if payload.len() > 1024 {
            return Err(zbus::fdo::Error::InvalidArgs("定位消息过长".into()));
        }
        let location = serde_json::from_str(payload)
            .map_err(|_| zbus::fdo::Error::InvalidArgs("定位消息无效".into()))?;
        self.send(Event::Located(location)).await
    }
    async fn frame(
        &self,
        identity: &str,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        let identity = decode(identity)?;
        self.send(Event::Painted(identity)).await
    }
    async fn action(
        &self,
        payload: &str,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        self.send(Event::Action(decode(payload)?)).await
    }
    async fn copied(
        &self,
        identity: &str,
        success: bool,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authenticate(conn, &header).await?;
        self.send(Event::Copied(decode(identity)?, success)).await
    }
    #[zbus(signal)]
    async fn capture_requested(emitter: &SignalEmitter<'_>, ticket: u64) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn probe_requested(emitter: &SignalEmitter<'_>, ticket: u64) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn view_changed(
        emitter: &SignalEmitter<'_>,
        view: &str,
        x: f64,
        y: f64,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn hidden(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn copy_requested(
        emitter: &SignalEmitter<'_>,
        identity: &str,
        text: &str,
    ) -> zbus::Result<()>;
}

fn decode<T: serde::de::DeserializeOwned>(text: &str) -> zbus::fdo::Result<T> {
    if text.len() > 256 {
        return Err(zbus::fdo::Error::InvalidArgs("操作消息过长".into()));
    }
    serde_json::from_str(text).map_err(|_| zbus::fdo::Error::InvalidArgs("操作消息无效".into()))
}

fn compositor() -> Result<(wayland_client::Connection, u32)> {
    let conn = wayland_client::Connection::connect_to_env()?;
    let peer = rustix::net::sockopt::socket_peercred(conn.backend().poll_fd())?;
    if peer.uid != rustix::process::geteuid() {
        bail!("GNOME compositor 不属于当前用户")
    }
    let pid = peer.pid.as_raw_nonzero().get() as u32;
    Ok((conn, pid))
}

pub(super) fn run(services: Services) -> Result<()> {
    let Some(owner) = services.control.claim()? else {
        return Ok(());
    };
    let controller = Controller::new(services.settings.clone())?;
    let (_wayland, pid) = compositor()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    let result = runtime.block_on(async {
        let (events, mut incoming) = mpsc::channel(16);
        let conn = Builder::session()?.name(BUS)?.serve_at(PATH, Port { pid, events: events.clone() })?.build().await?;
        let server = owner.bind()?;
        let controls = events.clone();
        tokio::spawn(async move { let _ = server.serve(Arc::new(move |message| { let _ = controls.try_send(Event::Control(message)); })).await; });
        let mut state = State { conn, events, controller, renderer: (services.panel)(),
            translator: (services.translation)()?, host: None, window: 0, epoch: 0, origin: None,
            task: None, capture_deadline: None, scene: 0, painted: None, probe: None, exit: false };
        eprintln!("Glosslet：已启动；GNOME Wayland 需启用 Glosslet Shell 扩展");
        while !state.exit {
            let deadline = state.capture_deadline.into_iter().chain(state.controller.session.ready_deadline()).min();
            tokio::select! {
                event = incoming.recv() => {
                    let Some(event) = event else { break };
                    if let Err(error) = state.event(event).await {
                        eprintln!("Glosslet：{error:#}");
                        state.close().await?;
                    }
                }
                _ = async {
                    if let Some(deadline) = deadline { tokio::time::sleep_until(deadline.into()).await; }
                    else { std::future::pending::<()>().await; }
                } => { state.close().await?; eprintln!("Glosslet：取词或面板首帧超时，本次查词取消"); }
            }
        }
        state.close().await?;
        Ok::<_, anyhow::Error>(())
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

struct State {
    conn: Connection,
    events: mpsc::Sender<Event>,
    translator: Arc<dyn Translator>,
    controller: Controller,
    renderer: Box<dyn Painter>,
    host: Option<OwnedUniqueName>,
    window: u64,
    epoch: u64,
    origin: Option<Origin>,
    task: Option<JoinHandle<()>>,
    capture_deadline: Option<Instant>,
    scene: u64,
    painted: Option<HostIdentity>,
    probe: Option<Ticket>,
    exit: bool,
}
impl State {
    fn emitter(&self) -> Result<SignalEmitter<'_>> {
        Ok(SignalEmitter::new(&self.conn, PATH)?)
    }
    async fn close(&mut self) -> Result<()> {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.controller.session.close();
        self.renderer.reset();
        self.painted = None;
        self.origin = None;
        self.capture_deadline = None;
        self.probe = None;
        self.scene += 1;
        Port::hidden(&self.emitter()?).await?;
        Ok(())
    }
    async fn redraw(&mut self) -> Result<()> {
        let Some(display) = self.controller.session.display() else {
            return Ok(());
        };
        let origin = self.origin.context("面板缺少原生定位")?;
        let position = self
            .renderer
            .place(
                (origin.point[0], origin.point[1]),
                (
                    origin.bounds[0],
                    origin.bounds[1],
                    origin.bounds[2],
                    origin.bounds[3],
                ),
            )
            .context("输出放不下固定面板或鼠标不属于该输出")?;
        let view = self.renderer.view(
            display,
            self.controller.session.pinned(),
            self.controller.settings().theme,
        );
        #[derive(Serialize)]
        struct HostView<'a> {
            scene: u64,
            #[serde(flatten)]
            view: &'a View,
        }
        self.scene += 1;
        self.painted = None;
        Port::view_changed(
            &self.emitter()?,
            &serde_json::to_string(&HostView {
                scene: self.scene,
                view: &view,
            })?,
            position.0,
            position.1,
        )
        .await?;
        Ok(())
    }
    async fn pin(&mut self, pinned: bool) -> Result<()> {
        if self.controller.session.pinned() == pinned {
            return Ok(());
        }
        if !self.painted.is_some_and(|identity| {
            identity.scene == self.scene
                && self.controller.session.content(identity.display).is_some()
        }) {
            bail!("面板隐藏或当前绘制尚未完成，无法改变固定状态");
        }
        self.controller.session.set_pinned(pinned);
        self.redraw().await
    }

    async fn command(&mut self, command: Command) -> Result<String> {
        if let Some(configured) = self.controller.configure(&command)? {
            if configured.redraw && self.origin.is_some() {
                self.redraw().await?;
            }
            return Ok(configured.reply);
        }
        match command {
            Command::Status => {
                return Ok(self.controller.status(
                    "GNOME",
                    Some(if self.host.is_some() {
                        "扩展：已连接"
                    } else {
                        "扩展：未连接"
                    }),
                ));
            }
            Command::Pin(pinned) => self.pin(pinned).await?,
            Command::Lookup => {
                if self.host.is_none() {
                    bail!("GNOME Shell 扩展未连接；请安装并启用 glosslet@ankroot")
                }
                if let Some(task) = self.task.take() {
                    task.abort();
                }
                let ticket = self.controller.capture();
                self.capture_deadline = Some(Instant::now() + Duration::from_millis(800));
                Port::capture_requested(&self.emitter()?, ticket.0).await?;
            }
            Command::ProbePanel => {
                if self.host.is_none() {
                    bail!("GNOME Shell 扩展未连接")
                }
                self.close().await?;
                let ticket = self.controller.session.probe_panel();
                self.probe = Some(ticket);
                self.capture_deadline = Some(Instant::now() + Duration::from_millis(800));
                Port::probe_requested(&self.emitter()?, ticket.0).await?;
            }
            Command::Close => self.close().await?,
            Command::Quit => {
                self.close().await?;
                self.exit = true;
            }
            Command::Settings | Command::Service(_) | Command::Direction(_) | Command::Theme(_) => {
                unreachable!("偏好命令已由共享控制器处理")
            }
        }
        Ok("已接收".into())
    }
    async fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Control(message) => {
                let result = self
                    .command(message.command)
                    .await
                    .map_err(|error| format!("{error:#}"));
                let _ = message.reply.send(result);
            }
            Event::Attach(host, window, epoch) => {
                self.close().await?;
                self.host = Some(host);
                self.window = window;
                self.epoch = epoch;
            }
            Event::Detach => {
                self.close().await?;
                self.host = None;
            }
            Event::Focus(window, epoch) => {
                if epoch <= self.epoch {
                    return Ok(());
                }
                self.window = window;
                self.epoch = epoch;
                if self.controller.session.capturing() {
                    self.controller.session.cancel_capture();
                    self.capture_deadline = None;
                }
                if self.origin.is_some_and(|origin| origin.epoch != epoch)
                    && (!self.controller.session.pinned()
                        || self.controller.session.ready_deadline().is_some())
                {
                    self.close().await?;
                }
            }
            Event::Captured(capture) => {
                if capture.origin.window != self.window || capture.origin.epoch != self.epoch {
                    return Ok(());
                }
                let text = match (capture.text, capture.failure.as_deref()) {
                    (Some(text), None) => Ok(text),
                    (None, Some("too_large")) => Err(Failure::SelectionTooLarge),
                    (None, Some("timeout")) => Err(Failure::CaptureTimeout),
                    (None, Some("invalid_text")) => Err(Failure::InvalidText),
                    (None, Some("unavailable")) => Err(Failure::CaptureUnavailable),
                    _ => bail!("GNOME 取词结果形状无效"),
                };
                match self.controller.session.captured(capture.ticket, text) {
                    CaptureResult::Ignored => {}
                    CaptureResult::Empty => {
                        self.capture_deadline = None;
                        if self.controller.session.display().is_none() {
                            self.close().await?;
                        }
                    }
                    _ => {
                        self.capture_deadline = None;
                        if !self.controller.session.pinned() || self.origin.is_none() {
                            self.origin = Some(capture.origin);
                        }
                        self.redraw().await?;
                    }
                }
            }
            Event::Located(location) => {
                if self.probe == Some(location.ticket) {
                    self.probe = None;
                    self.capture_deadline = None;
                    self.origin = Some(location.origin);
                    self.redraw().await?;
                }
            }
            Event::Painted(identity) => {
                if identity.scene != self.scene
                    || self.controller.session.content(identity.display).is_none()
                {
                    return Ok(());
                }
                self.painted = Some(identity);
                if let Some(request) = self
                    .controller
                    .session
                    .start_translation(identity.display, Instant::now())
                {
                    let translator = self.translator.clone();
                    let events = self.events.clone();
                    self.task = Some(tokio::spawn(async move {
                        let result = translator.translate(&request).await;
                        let _ = events.send(Event::Translated(request.ticket, result)).await;
                    }));
                }
            }
            Event::Translated(ticket, result) => {
                if self.controller.session.translated(ticket, result) {
                    self.task = None;
                    self.redraw().await?;
                }
            }
            Event::Copied(identity, success) => {
                if self.painted == Some(identity)
                    && self.controller.session.content(identity.display).is_some()
                    && success
                    && !self.controller.session.pinned()
                {
                    self.close().await?;
                }
            }
            Event::Action(action) => {
                if self.painted != Some(action.identity)
                    || self
                        .controller
                        .session
                        .content(action.identity.display)
                        .is_none()
                {
                    return Ok(());
                }
                match action.kind {
                    ActionKind::Close => self.close().await?,
                    ActionKind::Pin => self.pin(!self.controller.session.pinned()).await?,
                    ActionKind::Theme => {
                        if let Err(error) = self.controller.toggle_theme() {
                            eprintln!("Glosslet：无法切换主题：{error:#}");
                        } else {
                            self.redraw().await?;
                        }
                    }
                    ActionKind::Retry => {
                        if self
                            .controller
                            .session
                            .retry(action.identity.display)
                            .is_some()
                        {
                            self.redraw().await?;
                        }
                    }
                    ActionKind::Term | ActionKind::Sentence => {
                        let display = self.controller.session.display().unwrap();
                        let text = match (&action.kind, &display.content) {
                            (ActionKind::Sentence, Content::Ready(Translation::Sentence(text))) => {
                                Some(text.clone())
                            }
                            (ActionKind::Term, Content::Ready(Translation::Dictionary(_))) => {
                                let view = self.renderer.view(
                                    display,
                                    self.controller.session.pinned(),
                                    self.controller.settings().theme,
                                );
                                action
                                    .index
                                    .and_then(|index| view.lines.get(index))
                                    .filter(|line| line.copy)
                                    .map(|line| line.text.clone())
                            }
                            _ => None,
                        };
                        if let Some(text) = text {
                            Port::copy_requested(
                                &self.emitter()?,
                                &serde_json::to_string(&action.identity)?,
                                &text,
                            )
                            .await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

pub(super) fn check() -> Result<()> {
    let (_wayland, pid) = compositor()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let conn = Connection::session().await?;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let owner = dbus.get_name_owner("org.gnome.Shell".try_into()?).await?;
        if dbus.get_connection_unix_process_id(owner.into()).await? != pid { bail!("GNOME Shell 与 Wayland compositor 不是同一进程") }
        let proxy = zbus::Proxy::new(&conn, "org.gnome.Shell", "/org/gnome/Shell/Extensions", "org.gnome.Shell.Extensions").await?;
        let info: std::collections::HashMap<String, zbus::zvariant::OwnedValue> = proxy.call("GetExtensionInfo", &(UUID,)).await?;
        let state = info.get("state").and_then(|value| u32::try_from(value).ok());
        if state != Some(1) { bail!("Glosslet GNOME Shell 扩展未安装或未启用") }
        println!("桌面：GNOME；Shell 与 Wayland 身份一致，扩展已启用；未读取选区、未映射窗口、未联系 Glosslet 实例。");
        Ok(())
    })
}
