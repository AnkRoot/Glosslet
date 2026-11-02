//! 原生 Wayland 查词装配。只编排意图、来源与 I/O，面板和选区各自拥有资源。

mod desktop;
mod panel;
mod selection;

use super::{controller::Controller, session::CaptureResult};
use crate::core::{
    Command, ControlLease, ControlRequest, Failure, Request, Services, Ticket, Translation,
    Translator,
};
use anyhow::{Context, Result, bail};
use calloop::{EventLoop, channel};
use calloop_wayland_source::WaylandSource;
use panel::{Presenter, logical_bounds, output_local};
use selection::{Device, Manager, Offer, OfferData, Selection, Source, SourceData, read_selection};
use smithay_client_toolkit::{
    compositor::CompositorHandler,
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_relative_pointer, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerHandler},
        relative_pointer::{RelativeMotionEvent, RelativePointerHandler},
    },
    shell::wlr_layer::{LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    shm::{Shm, ShmHandler},
};
use std::{
    fs::File,
    io::{self, Write},
    os::unix::net::UnixStream,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tokio::{
    io::unix::AsyncFd,
    runtime::Handle,
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_surface},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_offer_v1, ext_data_control_source_v1,
};
use wayland_protocols::wp::{
    relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1,
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

pub(super) fn run(services: Services) -> Result<()> {
    let Some(owner) = services.control.claim()? else {
        return Ok(());
    };

    let controller = Controller::new(services.settings.clone())?;
    let conn = Connection::connect_to_env().context("无法连接原生 Wayland；不使用 X11 回退")?;
    let desktop = desktop::Broker::detect(compositor_pid(&conn)?)?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    require_protocols(&globals)?;
    let qh = queue.handle();
    let presenter = Presenter::bind(&globals, &qh, (services.panel)())?;
    let selection = Selection::bind(&globals, &qh)?;
    let mut event_loop = EventLoop::<State>::try_new()?;
    let (sender, receiver) = channel::channel();
    let worker = Worker::new(sender, (services.translation)()?)?;
    worker.watch_focus(desktop.clone());
    worker.control(owner)?;
    let mut state = State {
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        qh: qh.clone(),
        desktop,
        worker,
        presenter,
        selection,
        controller,
        task: None,
        location: Location::Idle,
        capture: None,
        display_source: None,
        focus: desktop::Focus::default(),
        source_epoch: 0,
        seat: None,
        pointer: None,
        keyboard: None,
        exit: false,
        failure: None,
    };
    // SCTK 不再回调构造期间的初始座位；初始和热插拔走同一个接入点。
    for seat in state.seats.seats().collect::<Vec<_>>() {
        state.new_seat(&conn, &qh, seat);
    }
    if let Some(failure) = &state.failure {
        bail!("{failure}");
    }
    queue.roundtrip(&mut state)?;
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;
    event_loop
        .handle()
        .insert_source(receiver, |event, _, state| {
            if let channel::Event::Msg(event) = event {
                state.worker_event(event);
            }
        })
        .map_err(|error| error.error)?;
    eprintln!("Glosslet：已启动；快捷键执行 glosslet lookup");
    while !state.exit {
        let timeout = state
            .deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        event_loop.dispatch(timeout, &mut state)?;
        if state
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            state.close();
            eprintln!("Glosslet：定位、首帧或加载展示超时；本次查词已取消");
        }
        state.presenter.draw(
            state.controller.session.display(),
            state.controller.session.pinned(),
            state.controller.settings().theme,
        )?;
        if let Some(frame) = state.presenter.painted()
            && let Some(request) = state
                .controller
                .session
                .start_translation(frame, Instant::now())
        {
            state.task = Some(Task(state.worker.translate(request)));
        }
    }
    if let Some(failure) = state.failure {
        bail!("{failure}");
    }
    Ok(())
}

fn require_protocols(globals: &GlobalList) -> Result<()> {
    let available = globals.contents().with_list(|list| {
        [
            ("zwlr_layer_shell_v1", 4),
            ("wp_viewporter", 1),
            ("ext_data_control_manager_v1", 1),
        ]
        .into_iter()
        .all(|(interface, version)| {
            list.iter()
                .any(|global| global.interface == interface && global.version >= version)
        })
    });
    if !available {
        bail!("桌面未公布完整的 layer-shell v4、viewporter 与 ext-data-control 协议");
    }
    Ok(())
}

fn compositor_pid(conn: &Connection) -> Result<i32> {
    let peer = rustix::net::sockopt::socket_peercred(conn.backend().poll_fd())
        .context("无法确认 Wayland compositor 身份")?;
    if peer.uid != rustix::process::geteuid() {
        bail!("Wayland compositor 不属于当前用户");
    }
    Ok(peer.pid.as_raw_nonzero().get())
}

/// 只检查协议公告与桌面 IPC；不绑定选区设备，不映射窗口，不联系运行实例。
pub(super) fn check_desktop() -> Result<()> {
    let conn = Connection::connect_to_env().context("无法连接原生 Wayland")?;
    let broker = desktop::Broker::detect(compositor_pid(&conn)?)?;
    let (globals, _queue) = registry_queue_init::<State>(&conn)?;
    require_protocols(&globals)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    let result = runtime.block_on(async {
        if broker.native_trigger() {
            broker
                .check_plugin()
                .await
                .map_err(|_| "KWin 脚本或桌面身份不可用")?;
            return Ok(());
        }
        let mut window = None;
        broker
            .watch(
                |update| {
                    if let desktop::Update::Changed(focused) = update {
                        window = focused;
                    }
                    false
                },
                |_| false,
            )
            .await?;
        if broker.native_anchor() {
            broker.anchor(window).await?;
        }
        Ok::<_, &'static str>(())
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    result.map_err(|error| anyhow::anyhow!(error))?;
    println!(
        "桌面：{}；所需协议已公布；IPC 与 Wayland 同进程身份已验证；原生鼠标锚点：{}",
        broker.name(),
        broker.native_anchor()
    );
    println!("未读取选区、未映射面板；这不是选区所有者证明或完整桌面验收。");
    Ok(())
}

enum WorkerEvent {
    Control(ControlRequest),
    Focus(desktop::Update),
    NativeLookup(desktop::NativeTrigger),
    TriggerFailed(Ticket, &'static str),
    FocusUnavailable(&'static str),
    Anchored {
        ticket: Ticket,
        source: Option<desktop::Snapshot>,
        result: std::result::Result<(f64, f64), &'static str>,
    },
    Captured {
        ticket: Ticket,
        epoch: u64,
        result: std::result::Result<String, Failure>,
    },
    Translated {
        ticket: Ticket,
        result: std::result::Result<Translation, Failure>,
    },
}

struct Worker {
    runtime: Handle,
    events: channel::Sender<WorkerEvent>,
    translator: Arc<dyn Translator>,
    copy_slots: Arc<Semaphore>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn new(events: channel::Sender<WorkerEvent>, translator: Arc<dyn Translator>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .thread_keep_alive(Duration::from_secs(1))
            .build()?;
        let handle = runtime.handle().clone();
        let (shutdown, receive) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("glosslet-io".into())
            .spawn(move || {
                runtime.block_on(async {
                    let _ = receive.await;
                });
                // 系统 DNS 可能仍在阻塞；退出不无限等待它。
                runtime.shutdown_timeout(Duration::from_secs(1));
            })?;
        Ok(Self {
            runtime: handle,
            events,
            translator,
            copy_slots: Arc::new(Semaphore::new(4)),
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    fn control(&self, owner: Box<dyn ControlLease>) -> Result<()> {
        let _enter = self.runtime.enter();
        let server = owner.bind()?;
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if server
                .serve(Arc::new(move |message| {
                    let _ = events.send(WorkerEvent::Control(message));
                }))
                .await
                .is_err()
            {
                eprintln!("Glosslet：控制入口已失效");
            }
        });
        Ok(())
    }

    fn watch_focus(&self, desktop: desktop::Broker) {
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(error) = desktop
                .watch(
                    |update| events.send(WorkerEvent::Focus(update)).is_ok(),
                    |trigger| events.send(WorkerEvent::NativeLookup(trigger)).is_ok(),
                )
                .await
            {
                let _ = events.send(WorkerEvent::FocusUnavailable(error));
            }
        });
    }

    fn invoke_lookup(&self, ticket: Ticket, desktop: desktop::Broker) -> JoinHandle<()> {
        let events = self.events.clone();
        self.runtime.spawn(async move {
            let result =
                tokio::time::timeout(Duration::from_millis(800), desktop.invoke_lookup()).await;
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error,
                Err(_) => "KWin 动作调用超时",
            };
            let _ = events.send(WorkerEvent::TriggerFailed(ticket, error));
        })
    }

    fn anchor(
        &self,
        ticket: Ticket,
        source: Option<desktop::Snapshot>,
        desktop: desktop::Broker,
    ) -> JoinHandle<()> {
        let events = self.events.clone();
        self.runtime.spawn(async move {
            let result = desktop.anchor(source.map(|source| source.window)).await;
            let _ = events.send(WorkerEvent::Anchored {
                ticket,
                source,
                result,
            });
        })
    }

    fn capture(&self, ticket: Ticket, epoch: u64, read: UnixStream) -> JoinHandle<()> {
        let events = self.events.clone();
        self.runtime.spawn(async move {
            let result = read_selection(read).await;
            let _ = events.send(WorkerEvent::Captured {
                ticket,
                epoch,
                result,
            });
        })
    }

    fn translate(&self, request: Request) -> JoinHandle<()> {
        let events = self.events.clone();
        let translator = self.translator.clone();
        self.runtime.spawn(async move {
            let result = translator.translate(&request).await;
            let _ = events.send(WorkerEvent::Translated {
                ticket: request.ticket,
                result,
            });
        })
    }

    fn copy_to(&self, fd: std::os::fd::OwnedFd, text: Arc<str>) {
        let Ok(permit) = self.copy_slots.clone().try_acquire_owned() else {
            return;
        };
        self.runtime.spawn(async move {
            let _permit = permit;
            let operation = async {
                let file = File::from(fd);
                let flags = rustix::fs::fcntl_getfl(&file)?;
                rustix::fs::fcntl_setfl(&file, flags | rustix::fs::OFlags::NONBLOCK)?;
                // SAFETY: File 独占拥有该 fd；注册期间不替换、关闭或泄漏它。
                let fd = unsafe { AsyncFd::register(file) }
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let mut bytes = text.as_bytes();
                while !bytes.is_empty() {
                    let mut ready = fd.writable().await?;
                    if let Ok(result) = ready.try_io(|fd| fd.get_ref().write(bytes)) {
                        let written = result?;
                        if written == 0 {
                            return Err(io::ErrorKind::WriteZero.into());
                        }
                        bytes = &bytes[written..];
                    }
                }
                Ok::<_, io::Error>(())
            };
            if !matches!(
                tokio::time::timeout(Duration::from_secs(1), operation).await,
                Ok(Ok(()))
            ) {
                eprintln!("Glosslet：剪贴板数据传输失败");
            }
        });
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 任务属于当前阶段，离开阶段即取消，迟到消息仍由身份校验拒绝。
struct Task(JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Copy)]
struct CaptureContext {
    ticket: Ticket,
    epoch: u64,
    source: desktop::Snapshot,
}

enum Location {
    Idle,
    Waiting {
        ticket: Ticket,
        source: Option<desktop::Snapshot>,
        _task: Task,
    },
    Ready {
        ticket: Ticket,
        point: (f64, f64),
    },
}

impl Location {
    fn begin(&mut self, ticket: Ticket, source: Option<desktop::Snapshot>, task: Task) {
        *self = Self::Waiting {
            ticket,
            source,
            _task: task,
        };
    }
    fn cancel(&mut self) {
        *self = Self::Idle;
    }
    fn active(&self) -> bool {
        !matches!(self, Self::Idle)
    }
    fn receive(
        &mut self,
        ticket: Ticket,
        source: Option<desktop::Snapshot>,
        current: Option<desktop::Snapshot>,
        result: Result<(f64, f64), &'static str>,
    ) -> Option<Result<(), &'static str>> {
        if !matches!(self, Self::Waiting { ticket: expected, source: origin, .. } if *expected == ticket && *origin == source)
        {
            return None;
        }
        self.cancel();
        if source.is_some() && current != source {
            return Some(Err("鼠标锚点不再属于触发时的来源，请重新触发"));
        }
        Some(result.map(|point| *self = Self::Ready { ticket, point }))
    }
    fn take(&mut self, ticket: Ticket) -> Option<(f64, f64)> {
        let Self::Ready {
            ticket: expected,
            point,
        } = self
        else {
            return None;
        };
        if *expected != ticket {
            return None;
        }
        let point = *point;
        self.cancel();
        Some(point)
    }
}

struct State {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    qh: QueueHandle<Self>,
    desktop: desktop::Broker,
    worker: Worker,
    presenter: Presenter,
    selection: Selection,
    controller: Controller,
    task: Option<Task>,
    location: Location,
    capture: Option<CaptureContext>,
    display_source: Option<u128>,
    focus: desktop::Focus,
    source_epoch: u64,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    exit: bool,
    failure: Option<String>,
}

impl State {
    fn deadline(&self) -> Option<Instant> {
        self.controller
            .session
            .ready_deadline()
            .into_iter()
            .chain(self.presenter.deadline())
            .min()
    }

    fn close(&mut self) {
        self.task = None;
        self.location.cancel();
        self.capture = None;
        self.display_source = None;
        self.controller.session.close();
        self.presenter.hide();
    }

    fn cancel_capture(&mut self) {
        if !self.controller.session.cancel_capture() {
            return;
        }
        self.task = None;
        self.location.cancel();
        self.capture = None;
        self.presenter.cancel_positioning();
        if !self
            .controller
            .session
            .display()
            .is_some_and(|display| self.presenter.contains_ticket(display.ticket))
        {
            self.close();
        }
        self.presenter.redraw();
    }

    fn fail(&mut self, error: impl std::fmt::Display) {
        self.failure = Some(error.to_string());
        self.close();
        self.exit = true;
    }

    fn pin(&mut self, pinned: bool) -> Result<()> {
        if self.controller.session.pinned() == pinned {
            return Ok(());
        }
        if !self
            .presenter
            .painted()
            .is_some_and(|identity| self.controller.session.content(identity).is_some())
        {
            bail!("面板隐藏或当前绘制尚未完成，无法改变固定状态");
        }
        self.controller.session.set_pinned(pinned);
        if pinned {
            self.location.cancel();
        } else if self.desktop.native_anchor()
            && let Some(context) = self.capture
        {
            self.start_anchor(context.ticket, Some(context.source));
        }
        self.presenter.redraw();
        Ok(())
    }

    fn command(&mut self, command: Command) -> Result<String> {
        if let Some(configured) = self.controller.configure(&command)? {
            if configured.redraw {
                self.presenter.redraw();
            }
            return Ok(configured.reply);
        }
        match command {
            Command::Status => return Ok(self.controller.status(self.desktop.name(), None)),
            Command::Pin(pinned) => self.pin(pinned)?,
            Command::Lookup => {
                if self.desktop.native_trigger() {
                    self.task = None;
                    self.location.cancel();
                    self.capture = None;
                    let ticket = self.controller.capture();
                    self.task = Some(Task(
                        self.worker.invoke_lookup(ticket, self.desktop.clone()),
                    ));
                } else {
                    self.lookup(None);
                }
            }
            Command::Close => self.close(),
            Command::Quit => {
                self.close();
                self.exit = true;
            }
            Command::Settings | Command::Service(_) | Command::Direction(_) | Command::Theme(_) => {
                unreachable!("偏好命令已由共享控制器处理")
            }
            Command::ProbePanel => {
                self.close();
                let ticket = self.controller.session.probe_panel();
                self.display_source = self.focus.snapshot().map(|source| source.window);
                if self.desktop.native_anchor() {
                    self.start_anchor(ticket, None);
                } else if let Err(error) = self.sample(ticket) {
                    self.fail(error);
                }
            }
        }
        Ok("已接收".into())
    }

    fn start_anchor(&mut self, ticket: Ticket, source: Option<desktop::Snapshot>) {
        let task = Task(self.worker.anchor(ticket, source, self.desktop.clone()));
        self.location.begin(ticket, source, task);
    }

    fn lookup(&mut self, native_point: Option<(f64, f64)>) {
        self.task = None;
        self.location.cancel();
        self.presenter.cancel_positioning();
        let ticket = self.controller.capture();
        if self.presenter.focused() || self.pointer.is_none() {
            self.cancel_capture();
            return;
        }
        let Some(source) = self.focus.snapshot() else {
            self.cancel_capture();
            eprintln!("Glosslet：无法确认活动来源窗口；请在原应用中重新触发");
            return;
        };
        self.capture = Some(CaptureContext {
            ticket,
            epoch: self.source_epoch,
            source,
        });
        let read = self.selection.read();
        if matches!(read, Ok(None)) {
            self.captured(ticket, self.source_epoch, Ok(String::new()));
            return;
        }
        if !self.controller.session.pinned() {
            if let Some(point) = native_point {
                self.location = Location::Ready { ticket, point };
            } else if self.desktop.native_anchor() {
                self.start_anchor(ticket, Some(source));
            }
        }
        match read {
            Ok(Some(read)) => {
                self.task = Some(Task(self.worker.capture(ticket, self.source_epoch, read)))
            }
            Err(error) => self.captured(ticket, self.source_epoch, Err(error)),
            Ok(None) => unreachable!(),
        }
    }

    fn captured(&mut self, ticket: Ticket, epoch: u64, result: Result<String, Failure>) {
        let Some(context) = self.capture else {
            return;
        };
        if context.ticket != ticket
            || context.epoch != epoch
            || self.source_epoch != epoch
            || self.focus.snapshot() != Some(context.source)
        {
            return;
        }
        let outcome = self.controller.session.captured(ticket, result);
        if outcome == CaptureResult::Ignored {
            return;
        }
        self.task = None;
        self.capture = None;
        if outcome == CaptureResult::Empty {
            self.location.cancel();
            self.presenter.cancel_positioning();
            if self.controller.session.display().is_none() && !self.controller.session.pinned() {
                self.close();
            } else {
                self.presenter.redraw();
            }
            return;
        }
        self.display_source = Some(context.source.window);
        self.present(ticket);
    }

    fn present(&mut self, ticket: Ticket) {
        if !self
            .controller
            .session
            .display()
            .is_some_and(|display| display.ticket == ticket)
        {
            return;
        }
        if self.controller.session.pinned() {
            self.location.cancel();
            self.presenter.reuse(ticket);
        } else if self.desktop.native_anchor() {
            if let Some(point) = self.location.take(ticket)
                && let Err(error) = self.map(ticket, point)
            {
                self.close();
                eprintln!("Glosslet：{error}；不使用采样面回退");
            }
        } else if let Err(error) = self.sample(ticket) {
            self.fail(error);
        }
    }

    fn map(&mut self, ticket: Ticket, point: (f64, f64)) -> Result<()> {
        let mut target = None;
        for output in self.outputs.outputs() {
            let info = self
                .outputs
                .info(&output)
                .context("Wayland 输出信息不可用")?;
            let origin = info.logical_position.context("桌面未提供输出逻辑位置")?;
            let bounds = info
                .logical_size
                .and_then(logical_bounds)
                .context("桌面未提供有效输出逻辑尺寸")?;
            if let Some(local) = output_local(point, origin, bounds) {
                if target.is_some() {
                    bail!("鼠标锚点属于多个重叠输出，不能猜测目标显示器");
                }
                target = Some((output, bounds, local));
            }
        }
        let (output, bounds, local) = target.context("鼠标锚点不属于当前有效输出")?;
        self.presenter.show(ticket, output, bounds, local)
    }

    fn sample(&mut self, ticket: Ticket) -> Result<()> {
        let outputs = self
            .outputs
            .outputs()
            .map(|output| {
                let bounds = self
                    .outputs
                    .info(&output)
                    .and_then(|info| info.logical_size)
                    .and_then(logical_bounds)
                    .context("桌面未提供有效输出逻辑尺寸")?;
                Ok((output, bounds))
            })
            .collect::<Result<Vec<_>>>()?;
        self.presenter.sample(ticket, outputs)
    }

    fn panel_event(&mut self, event: panel::Event) {
        match event {
            panel::Event::Anchored {
                ticket,
                output,
                bounds,
                point,
            } => {
                if !self.controller.session.pinned()
                    && self
                        .controller
                        .session
                        .display()
                        .is_some_and(|display| display.ticket == ticket)
                    && let Err(error) = self.presenter.show(ticket, output, bounds, point)
                {
                    self.close();
                    eprintln!("Glosslet：{error}");
                }
            }
            panel::Event::Action(action) => match action {
                panel::Action::Close => self.close(),
                panel::Action::Theme => {
                    if let Err(error) = self.controller.toggle_theme() {
                        eprintln!("Glosslet：无法切换主题：{error:#}");
                    } else {
                        self.presenter.redraw();
                    }
                }
                panel::Action::Pin => {
                    if let Err(error) = self.pin(!self.controller.session.pinned()) {
                        eprintln!("Glosslet：{error}");
                    }
                }
                panel::Action::Retry(identity) => {
                    if let Some(ticket) = self.controller.session.retry(identity) {
                        self.task = None;
                        self.presenter.reuse(ticket);
                    }
                }
                panel::Action::Copy(text) => {
                    if self.selection.copy(text, &self.qh) && !self.controller.session.pinned() {
                        self.close();
                    }
                }
            },
        }
    }

    fn output_changed(&mut self) {
        self.presenter.clear_input();
        if self.location.active() || self.presenter.positioning() {
            if self.controller.session.capturing() {
                self.cancel_capture();
            } else {
                self.close();
            }
            eprintln!("Glosslet：输出公告变化，本次定位或未确认映射已失效；请重新触发");
        }
    }

    fn worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Control(message) => {
                let result = self
                    .command(message.command)
                    .map_err(|error| format!("{error:#}"));
                let _ = message.reply.send(result);
            }
            WorkerEvent::NativeLookup(trigger) => {
                if self.focus.change(trigger.window) {
                    self.source_epoch += 1;
                }
                self.lookup(Some(trigger.point));
            }
            WorkerEvent::TriggerFailed(ticket, error) => {
                if self.controller.session.capture_ticket() == Some(ticket) {
                    self.cancel_capture();
                }
                eprintln!("Glosslet：{error}");
            }
            WorkerEvent::Captured {
                ticket,
                epoch,
                result,
            } => self.captured(ticket, epoch, result),
            WorkerEvent::Translated { ticket, result } => {
                if self.controller.session.translated(ticket, result) {
                    self.task = None;
                    self.presenter.redraw();
                }
            }
            WorkerEvent::FocusUnavailable(error) => self.fail(error),
            WorkerEvent::Focus(desktop::Update::Changed(window)) => {
                if self.focus.change(window) {
                    self.source_epoch += 1;
                    self.cancel_capture();
                    // None 也可能来自面板的按需焦点，不推测跨连接事件顺序。
                    if !self.controller.session.pinned()
                        && window.is_some_and(|window| {
                            self.display_source.is_some_and(|source| source != window)
                        })
                    {
                        self.close();
                    }
                }
            }
            WorkerEvent::Focus(desktop::Update::Closed(window)) => {
                if self
                    .capture
                    .is_some_and(|context| context.source.window == window)
                {
                    self.cancel_capture();
                }
                if !self.controller.session.pinned() && self.display_source == Some(window) {
                    self.close();
                }
            }
            WorkerEvent::Anchored {
                ticket,
                source,
                result,
            } => {
                if let Some(result) =
                    self.location
                        .receive(ticket, source, self.focus.snapshot(), result)
                {
                    match result {
                        Ok(()) => self.present(ticket),
                        Err(error) => {
                            if self.controller.session.capturing() {
                                self.cancel_capture();
                            } else {
                                self.close();
                            }
                            eprintln!("Glosslet：{error}；不猜测位置或更换定位路径");
                        }
                    }
                }
            }
        }
    }
}

impl Dispatch<Device, ()> for State {
    fn event(
        state: &mut Self,
        device: &Device,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match state.selection.device_event(device, event) {
            selection::Event::PrimaryChanged => {
                state.source_epoch += 1;
                state.cancel_capture();
            }
            selection::Event::Finished => state.fail("桌面数据控制服务已关闭"),
            selection::Event::None => {}
        }
    }
    wayland_client::event_created_child!(State, Device, [0 => (Offer, OfferData::default())]);
}
impl Dispatch<Offer, OfferData> for State {
    fn event(
        _: &mut Self,
        _: &Offer,
        event: ext_data_control_offer_v1::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        data.event(event);
    }
}
impl Dispatch<Source, SourceData> for State {
    fn event(
        state: &mut Self,
        source: &Source,
        event: ext_data_control_source_v1::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some((fd, text)) = state.selection.source_event(source, event, data) {
            state.worker.copy_to(fd, text);
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if let Err(error) = self.presenter.scale(surface, factor) {
            self.fail(error);
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
        self.presenter.clear_input();
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.presenter.frame(surface);
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.output_changed();
    }
    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.output_changed();
        if !self.presenter.contains_output(&output) {
            return;
        }
        let Some(bounds) = self
            .outputs
            .info(&output)
            .and_then(|info| info.logical_size)
            .and_then(logical_bounds)
        else {
            self.close();
            eprintln!("Glosslet：当前输出缺少有效逻辑几何");
            return;
        };
        if let Err(error) = self.presenter.resize(&output, bounds) {
            self.close();
            eprintln!("Glosslet：{error}");
        }
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.output_changed();
        if self.presenter.contains_output(&output) {
            self.close();
        }
    }
}
impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.presenter.closed(layer) {
            self.close();
        }
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.presenter.configure(layer);
    }
}
impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_some() {
            self.fail("当前尚不支持多座位；不混用焦点与选区");
            return;
        }
        self.selection.attach_seat(&seat, qh);
        self.seat = Some(seat);
    }
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        match capability {
            Capability::Pointer if self.pointer.is_none() => {
                match self.seats.get_pointer(qh, &seat) {
                    Ok(pointer) => self.pointer = Some(pointer),
                    Err(error) => self.fail(error),
                }
            }
            Capability::Keyboard if self.keyboard.is_none() => {
                match self.seats.get_keyboard(qh, &seat, None) {
                    Ok(keyboard) => self.keyboard = Some(keyboard),
                    Err(error) => self.fail(error),
                }
            }
            _ => {}
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        match capability {
            Capability::Pointer => {
                self.close();
                if let Some(pointer) = self.pointer.take() {
                    pointer.release();
                }
            }
            Capability::Keyboard => {
                self.close();
                if let Some(keyboard) = self.keyboard.take() {
                    keyboard.release();
                }
            }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        self.close();
        if let Some(pointer) = self.pointer.take() {
            pointer.release();
        }
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.release();
        }
        self.selection.detach_seat();
        self.seat = None;
        self.source_epoch += 1;
    }
}
impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if self.keyboard.as_ref() == Some(keyboard) && self.presenter.keyboard_enter(surface) {
            self.cancel_capture();
        }
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.keyboard.as_ref() == Some(keyboard)
            && self.presenter.keyboard_leave(surface)
            && !self.controller.session.pinned()
        {
            self.close();
        }
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.keyboard.as_ref() == Some(keyboard)
            && self.presenter.focused()
            && event.keysym == Keysym::Escape
        {
            self.close();
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: u32,
    ) {
    }
}
impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        if self.pointer.as_ref() != Some(pointer) {
            return;
        }
        for event in events {
            let content = self
                .presenter
                .painted()
                .and_then(|identity| self.controller.session.content(identity));
            if let Some(event) = self.presenter.pointer(pointer, event, content) {
                self.panel_event(event);
            }
        }
    }
}
impl RelativePointerHandler for State {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        relative: &ZwpRelativePointerV1,
        pointer: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        if self.pointer.as_ref() == Some(pointer) {
            self.presenter.relative_motion(relative, event);
        }
    }
}
impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        self.presenter.shm()
    }
}
impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_keyboard!(State);
delegate_pointer!(State);
delegate_relative_pointer!(State);
delegate_layer!(State);
delegate_registry!(State);
delegate_noop!(State: Manager);
delegate_noop!(State: WpViewporter);
delegate_noop!(State: WpViewport);
delegate_noop!(State: wl_region::WlRegion);

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    struct NoTranslation;
    impl Translator for NoTranslation {
        fn translate<'a>(&'a self, _: &'a Request) -> crate::core::TranslationFuture<'a> {
            Box::pin(async { panic!("传输测试不得读取选区或发起翻译") })
        }
    }

    #[tokio::test]
    async fn leaving_a_task_scope_closes_its_pending_transfer() {
        let (read, write) = UnixStream::pair().unwrap();
        write.set_nonblocking(true).unwrap();
        let mut peer = tokio::net::UnixStream::from_std(write).unwrap();
        let (ready, started) = oneshot::channel();
        let task = Task(tokio::spawn(async move {
            ready.send(()).unwrap();
            let _ = read_selection(read).await;
        }));
        started.await.unwrap();
        drop(task);
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn native_location_is_one_shot_and_rejects_replacement_cancel_and_aba() {
        let mut focus = desktop::Focus::default();
        focus.change(Some(1));
        let source = focus.snapshot();
        let mut location = Location::Idle;
        location.begin(
            Ticket(2),
            source,
            Task(tokio::spawn(std::future::pending())),
        );
        assert!(
            location
                .receive(Ticket(1), source, source, Ok((1.0, 2.0)))
                .is_none()
        );
        assert_eq!(
            location.receive(Ticket(2), source, source, Ok((10.0, 20.0))),
            Some(Ok(()))
        );
        assert!(location.take(Ticket(1)).is_none());
        assert_eq!(location.take(Ticket(2)), Some((10.0, 20.0)));
        assert!(location.take(Ticket(2)).is_none());
        location.begin(
            Ticket(3),
            source,
            Task(tokio::spawn(std::future::pending())),
        );
        focus.change(Some(2));
        focus.change(Some(1));
        assert!(matches!(
            location.receive(Ticket(3), source, focus.snapshot(), Ok((1.0, 2.0))),
            Some(Err(_))
        ));
        assert!(!location.active());
        location.begin(Ticket(4), None, Task(tokio::spawn(std::future::pending())));
        location.cancel();
        assert!(
            location
                .receive(Ticket(4), None, None, Ok((1.0, 2.0)))
                .is_none()
        );
    }

    #[tokio::test]
    async fn clipboard_transfer_owns_text_until_the_reader_finishes() {
        let (sender, _receiver) = channel::channel();
        let worker = Worker::new(sender, Arc::new(NoTranslation)).unwrap();
        let (read, write) = UnixStream::pair().unwrap();
        read.set_nonblocking(true).unwrap();
        let mut reader = tokio::net::UnixStream::from_std(read).unwrap();
        worker.copy_to(write.into(), Arc::from("copied translation"));
        let mut text = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_to_string(&mut text))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "copied translation");
    }
}
