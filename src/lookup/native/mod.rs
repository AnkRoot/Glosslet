//! Windows/macOS/X11 的事件驱动前端。取词只响应明确意图，软件面板提交后才发送原文。

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(target_os = "macos")]
use macos::Device;
#[cfg(target_os = "windows")]
use windows::Device;
#[cfg(target_os = "linux")]
use x11::Device;

use super::{controller::Controller, session::CaptureResult};
use crate::core::{
    Command, Content, ControlRequest, DisplayIdentity, Failure, Image, PANEL_HEADER, PANEL_HEIGHT,
    PANEL_WIDTH, Painter, Services, Ticket, Translation, Translator,
};
use anyhow::{Context, Result, bail};
use std::{num::NonZeroU32, sync::Arc, time::Instant};
use tokio::task::JoinHandle;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition},
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId, WindowLevel},
};

#[derive(Clone, Copy)]
pub(super) struct Origin {
    pub source: u64,
    pub point: (f64, f64),
    pub logical: bool,
}

pub(super) enum Event {
    Control(ControlRequest),
    Captured {
        ticket: Ticket,
        origin: Origin,
        text: std::result::Result<String, Failure>,
    },
    Focus(u64),
    SourceInvalidated,
    Translated(Ticket, std::result::Result<Translation, Failure>),
    #[cfg(target_os = "linux")]
    Failed(String),
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    Hotkey,
}

struct Painted {
    identity: DisplayIdentity,
    frame: u64,
    image: Image,
}

struct Press {
    identity: DisplayIdentity,
    frame: u64,
    point: (f64, f64),
    position: PhysicalPosition<i32>,
    moved: bool,
}

struct App {
    proxy: EventLoopProxy<Event>,
    runtime: tokio::runtime::Handle,
    translator: Arc<dyn Translator>,
    device: Device,
    controller: Controller,
    renderer: Box<dyn Painter>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    task: Option<JoinHandle<()>>,
    source: Option<u64>,
    painted: Option<Painted>,
    pressed: Option<Press>,
    click: Option<(DisplayIdentity, u64, (f64, f64), Instant)>,
    point: (f64, f64),
    scroll: f64,
    frame: u64,
    failure: Option<String>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    _hotkeys: (
        global_hotkey::GlobalHotKeyManager,
        global_hotkey::hotkey::HotKey,
    ),
}

pub(super) fn run(services: Services, shortcut: Option<&str>) -> Result<()> {
    let Some(owner) = services.control.claim()? else {
        return Ok(());
    };
    let controller = Controller::new(services.settings.clone())?;
    let mut builder = EventLoop::<Event>::with_user_event();
    #[cfg(target_os = "linux")]
    {
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_x11();
    }
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder
            .with_activation_policy(ActivationPolicy::Accessory)
            .with_activate_ignoring_other_apps(false);
    }
    let event_loop = builder.build()?;
    let proxy = event_loop.create_proxy();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()?;
    let server = {
        let _enter = runtime.enter();
        owner.bind()?
    };
    let controls = proxy.clone();
    runtime.spawn(async move {
        if let Err(error) = server
            .serve(Arc::new(move |message| {
                let _ = controls.send_event(Event::Control(message));
            }))
            .await
        {
            eprintln!("Glosslet：控制入口失效：{error}");
        }
    });
    let device = Device::new(proxy.clone(), runtime.handle())?;
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let hotkeys = register_shortcut(proxy.clone(), shortcut)?;
    #[cfg(target_os = "linux")]
    {
        let _ = shortcut;
    }
    let mut app = App {
        proxy,
        runtime: runtime.handle().clone(),
        translator: (services.translation)()?,
        device,
        controller,
        renderer: (services.panel)(),
        window: None,
        surface: None,
        task: None,
        source: None,
        painted: None,
        pressed: None,
        click: None,
        point: (0.0, 0.0),
        scroll: 0.0,
        frame: 0,
        failure: None,
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        _hotkeys: hotkeys,
    };
    eprintln!("Glosslet：已启动；仅明确组合键或 glosslet lookup 触发");
    event_loop.run_app(&mut app)?;
    let failure = app.failure.take();
    app.close();
    drop(app);
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    if let Some(error) = failure {
        bail!("{error}")
    }
    Ok(())
}

pub(super) fn check_desktop() -> Result<()> {
    Device::check()
}

impl App {
    fn invalidate(&mut self) {
        self.painted = None;
        self.pressed = None;
        self.click = None;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn close(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.device.cancel();
        self.controller.session.close();
        self.source = None;
        self.painted = None;
        self.pressed = None;
        self.click = None;
        self.scroll = 0.0;
        if let Some(window) = self.window.take() {
            self.device.hide(&window);
        }
        self.surface = None;
        self.renderer.reset();
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: impl std::fmt::Display) {
        self.failure = Some(error.to_string());
        self.close();
        event_loop.exit();
    }

    fn pin(&mut self, pinned: bool) -> Result<()> {
        if self.controller.session.pinned() == pinned {
            return Ok(());
        }
        if !self
            .painted
            .as_ref()
            .is_some_and(|painted| self.controller.session.content(painted.identity).is_some())
        {
            bail!("面板隐藏或当前绘制尚未完成，无法改变固定状态");
        }
        self.controller.session.set_pinned(pinned);
        self.invalidate();
        Ok(())
    }

    fn command(&mut self, event_loop: &ActiveEventLoop, command: Command) -> Result<String> {
        if let Some(configured) = self.controller.configure(&command)? {
            if configured.redraw {
                self.invalidate();
            }
            return Ok(configured.reply);
        }
        match command {
            Command::Status => return Ok(self.controller.status(Device::NAME, None)),
            Command::Pin(pinned) => self.pin(pinned)?,
            Command::Lookup => {
                if self
                    .window
                    .as_ref()
                    .is_some_and(|window| window.has_focus())
                {
                    bail!("请在原应用中选择并触发，不读取 Glosslet 自身选区");
                }
                if let Some(task) = self.task.take() {
                    task.abort();
                }
                self.device.cancel();
                let ticket = self.controller.capture();
                self.device.capture(ticket)?;
                self.invalidate();
            }
            Command::ProbePanel => {
                self.close();
                self.controller.session.probe_panel();
                self.show(event_loop, self.device.origin()?)?;
            }
            Command::Close => self.close(),
            Command::Quit => {
                self.close();
                event_loop.exit();
            }
            Command::Settings | Command::Service(_) | Command::Direction(_) | Command::Theme(_) => {
                unreachable!("偏好命令已由共享控制器处理")
            }
        }
        Ok("已接收".into())
    }

    fn show(&mut self, event_loop: &ActiveEventLoop, origin: Origin) -> Result<()> {
        if !self.controller.session.pinned() || self.window.is_none() {
            let mut target = None;
            for monitor in event_loop.available_monitors() {
                let scale = monitor.scale_factor();
                let position = monitor.position();
                let size = monitor.size();
                let factor = if origin.logical { 1.0 } else { scale };
                let point = (origin.point.0 / factor, origin.point.1 / factor);
                let bounds = (
                    position.x as f64 / scale,
                    position.y as f64 / scale,
                    size.width as f64 / scale,
                    size.height as f64 / scale,
                );
                if let Some(position) = self.renderer.place(point, bounds) {
                    if target.is_some() {
                        bail!("鼠标属于重叠输出，不猜测目标显示器")
                    }
                    target = Some((position, scale));
                }
            }
            let (position, scale) = target.context("目标输出不可用或放不下 256×100 面板")?;
            if self.window.is_none() {
                let attributes = Window::default_attributes()
                    .with_title("Glosslet")
                    .with_inner_size(LogicalSize::new(PANEL_WIDTH, PANEL_HEIGHT))
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_visible(false)
                    .with_active(false)
                    .with_window_level(WindowLevel::AlwaysOnTop);
                #[cfg(target_os = "linux")]
                let attributes = {
                    use winit::platform::x11::{WindowAttributesExtX11, WindowType};
                    attributes
                        .with_name(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_NAME"))
                        .with_override_redirect(true)
                        .with_x11_window_type(vec![WindowType::Utility])
                };
                let window = Arc::new(event_loop.create_window(attributes)?);
                let context = softbuffer::Context::new(window.clone())
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                self.surface = Some(
                    softbuffer::Surface::new(&context, window.clone())
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?,
                );
                self.window = Some(window);
            }
            let window = self.window.as_ref().unwrap();
            if origin.logical {
                window.set_outer_position(LogicalPosition::new(position.0, position.1));
            } else {
                window.set_outer_position(PhysicalPosition::new(
                    (position.0 * scale).round() as i32,
                    (position.1 * scale).round() as i32,
                ));
            }
            let _ = window.request_inner_size(LogicalSize::new(PANEL_WIDTH, PANEL_HEIGHT));
            self.scroll = 0.0;
        }
        let window = self.window.as_ref().unwrap();
        self.device.show(window)?;
        self.invalidate();
        Ok(())
    }

    fn paint(&mut self) -> Result<()> {
        let (Some(display), Some(window), Some(surface)) = (
            self.controller.session.display(),
            &self.window,
            &mut self.surface,
        ) else {
            return Ok(());
        };
        if window.is_visible() != Some(true) {
            return Ok(());
        }
        let size = window.inner_size();
        let scale = window.scale_factor();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        if (size.width as f64 / scale - PANEL_WIDTH).abs() > 1.0
            || (size.height as f64 / scale - PANEL_HEIGHT).abs() > 1.0
        {
            bail!("桌面改变了固定面板尺寸")
        }
        surface
            .resize(
                NonZeroU32::new(size.width).unwrap(),
                NonZeroU32::new(size.height).unwrap(),
            )
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let view = self.renderer.view(
            display,
            self.controller.session.pinned(),
            self.controller.settings().theme,
        );
        let image = self
            .renderer
            .render(&view, (size.width, size.height), scale, self.scroll);
        let mut buffer = surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        buffer.copy_from_slice(&image.pixels);
        buffer
            .present()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        self.frame += 1;
        self.painted = Some(Painted {
            identity: display.identity(),
            frame: self.frame,
            image,
        });
        if let Some(request) = self
            .controller
            .session
            .start_translation(display.identity(), Instant::now())
        {
            let translator = self.translator.clone();
            let proxy = self.proxy.clone();
            self.task = Some(self.runtime.spawn(async move {
                let result = translator.translate(&request).await;
                let _ = proxy.send_event(Event::Translated(request.ticket, result));
            }));
        }
        Ok(())
    }

    fn press(&mut self) -> Result<()> {
        let window = self.window.as_ref().context("面板尚未创建")?;
        if let Some(painted) = &self.painted {
            self.pressed = Some(Press {
                identity: painted.identity,
                frame: painted.frame,
                point: self.point,
                position: window.outer_position()?,
                moved: false,
            });
        }
        self.device.focus(window)
    }

    fn activate(&mut self) -> Result<()> {
        let Some(press) = self.pressed.take() else {
            return Ok(());
        };
        let Some(painted) = &self.painted else {
            return Ok(());
        };
        if press.moved
            || press.identity != painted.identity
            || press.frame != painted.frame
            || (self.point.0 - press.point.0).hypot(self.point.1 - press.point.1) > 3.0
        {
            return Ok(());
        }
        if self.point.1 < PANEL_HEADER {
            if self.point.0 < 55.0 {
                self.pin(!self.controller.session.pinned())?;
                return Ok(());
            } else if self.point.0 > 230.0 {
                self.close();
                return Ok(());
            } else if self.point.0 > 175.0
                && let Err(error) = self.controller.toggle_theme()
            {
                eprintln!("Glosslet：无法切换主题：{error:#}");
                return Ok(());
            }
            self.invalidate();
            return Ok(());
        }
        let Some(content) = self.controller.session.content(painted.identity) else {
            return Ok(());
        };
        let text = match content {
            Content::Ready(Translation::Dictionary(_)) => painted
                .image
                .terms
                .iter()
                .find(|term| (term.top..term.bottom).contains(&self.point.1))
                .map(|term| term.text.clone()),
            Content::Ready(Translation::Sentence(text)) => {
                let now = Instant::now();
                let double = self.click.is_some_and(|(identity, frame, point, at)| {
                    identity == painted.identity
                        && frame == painted.frame
                        && now.duration_since(at).as_millis() <= 500
                        && (point.0 - self.point.0).hypot(point.1 - self.point.1) <= 4.0
                });
                self.click = if double {
                    None
                } else {
                    Some((painted.identity, painted.frame, self.point, now))
                };
                double.then(|| text.clone())
            }
            Content::Failed { retry: Some(_), .. } => {
                if self.controller.session.retry(painted.identity).is_some() {
                    self.invalidate();
                }
                None
            }
            _ => None,
        };
        if let Some(text) = text {
            self.device.copy(text, self.window.as_ref().unwrap())?;
            if !self.controller.session.pinned() {
                self.close();
            }
        }
        Ok(())
    }
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        let result = match event {
            Event::Control(message) => {
                let result = self
                    .command(event_loop, message.command)
                    .map_err(|error| format!("{error:#}"));
                let _ = message.reply.send(result);
                Ok(())
            }
            Event::Captured {
                ticket,
                origin,
                text,
            } => match self.controller.session.captured(ticket, text) {
                CaptureResult::Ignored => Ok(()),
                CaptureResult::Empty => {
                    if self.controller.session.display().is_none() {
                        self.close();
                    }
                    Ok(())
                }
                _ => {
                    self.source = Some(origin.source);
                    self.show(event_loop, origin)
                }
            },
            Event::Focus(source) => {
                let own = self
                    .window
                    .as_ref()
                    .is_some_and(|window| self.device.owns(source, window));
                if !own
                    && self.source.is_some_and(|expected| source != expected)
                    && (!self.controller.session.pinned()
                        || self.controller.session.ready_deadline().is_some())
                {
                    self.close();
                }
                Ok(())
            }
            Event::SourceInvalidated => {
                if !self.controller.session.pinned()
                    || self.controller.session.ready_deadline().is_some()
                {
                    self.close();
                }
                Ok(())
            }
            Event::Translated(ticket, result) => {
                if self.controller.session.translated(ticket, result) {
                    self.task = None;
                    self.invalidate();
                }
                Ok(())
            }
            #[cfg(target_os = "linux")]
            Event::Failed(error) => Err(anyhow::anyhow!(error)),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Event::Hotkey => self.command(event_loop, Command::Lookup).map(|_| ()),
        };
        if let Err(error) = result {
            self.fail(event_loop, error);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let result = match event {
            WindowEvent::CloseRequested => {
                self.close();
                Ok(())
            }
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.invalidate();
                Ok(())
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                self.close();
                Ok(())
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.window.as_ref().unwrap().scale_factor();
                self.point = (position.x / scale, position.y / scale);
                if let Some(press) = &mut self.pressed {
                    let delta = (self.point.0 - press.point.0, self.point.1 - press.point.1);
                    press.moved |= delta.0.hypot(delta.1) > 3.0;
                    if press.moved
                        && press.point.1 < PANEL_HEADER
                        && (55.0..=175.0).contains(&press.point.0)
                    {
                        self.window
                            .as_ref()
                            .unwrap()
                            .set_outer_position(PhysicalPosition::new(
                                press.position.x + (delta.0 * scale) as i32,
                                press.position.y + (delta.1 * scale) as i32,
                            ));
                    }
                }
                Ok(())
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                if state == ElementState::Pressed {
                    self.press()
                } else {
                    self.activate()
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(painted) = &self.painted {
                    let delta = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y as f64 * 24.0,
                        MouseScrollDelta::PixelDelta(point) => {
                            point.y / self.window.as_ref().unwrap().scale_factor()
                        }
                    };
                    self.scroll = (self.scroll - delta).clamp(
                        0.0,
                        (painted.image.height - PANEL_HEIGHT + PANEL_HEADER + 4.0).max(0.0),
                    );
                    self.invalidate();
                }
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.fail(event_loop, error);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(deadline) = self.controller.session.ready_deadline() {
            if Instant::now() >= deadline {
                self.close();
                eprintln!("Glosslet：面板加载展示超时，本次查词取消");
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                return;
            }
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn register_shortcut(
    proxy: EventLoopProxy<Event>,
    shortcut: Option<&str>,
) -> Result<(
    global_hotkey::GlobalHotKeyManager,
    global_hotkey::hotkey::HotKey,
)> {
    use global_hotkey::{
        GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
        hotkey::{Code, HotKey, Modifiers},
    };
    let source: serde_json::Value =
        serde_json::from_str(include_str!("../../../integration/desktops.json"))?;
    let default = source["shortcut"]["modifiers"]
        .as_array()
        .context("默认快捷键结构无效")?
        .iter()
        .map(|v| v.as_str().unwrap())
        .chain(std::iter::once(source["shortcut"]["key"].as_str().unwrap()))
        .collect::<Vec<_>>()
        .join("+");
    let text = shortcut.unwrap_or(&default);
    let parts: Vec<_> = text.split('+').collect();
    let mut modifiers = Modifiers::empty();
    for part in &parts[..parts.len().saturating_sub(1)] {
        let flag = match *part {
            "Super" => Modifiers::SUPER,
            "Control" => Modifiers::CONTROL,
            "Alt" => Modifiers::ALT,
            "Shift" => Modifiers::SHIFT,
            _ => bail!("快捷键修饰符无效"),
        };
        if modifiers.contains(flag) {
            bail!("快捷键修饰符重复")
        }
        modifiers |= flag;
    }
    let key = parts.last().context("缺少快捷键")?;
    if key.len() != 1
        || !key.as_bytes()[0].is_ascii_alphabetic()
        || !modifiers.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT)
    {
        bail!("快捷键须为明确的组合键和一个英文字母")
    }
    let code: Code = format!("Key{}", key.to_ascii_uppercase())
        .parse()
        .map_err(|_| anyhow::anyhow!("快捷键按键无效"))?;
    let hotkey = HotKey::new(Some(modifiers), code);
    let manager = GlobalHotKeyManager::new()?;
    manager
        .register(hotkey)
        .context("组合键已被占用或不可注册；不扫描全局输入")?;
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.id == hotkey.id() && event.state == HotKeyState::Released {
            let _ = proxy.send_event(Event::Hotkey);
        }
    }));
    Ok((manager, hotkey))
}
