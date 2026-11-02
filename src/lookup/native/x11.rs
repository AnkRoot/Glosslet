//! X11 PRIMARY 的事件驱动转换。每次取词独占 requestor，迟到通知不能授权新选区。

use super::{Event, Origin};
use crate::core::{Failure, MAX_SELECTION_BYTES, Ticket};
use anyhow::{Context, Result, bail};
use std::{
    os::fd::AsRawFd,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::unix::AsyncFd,
    runtime::Handle,
    sync::{Notify, mpsc},
};
use winit::{
    event_loop::EventLoopProxy,
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    window::Window,
};
use x11rb::{
    COPY_FROM_PARENT, CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{Event as XEvent, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

struct Fd(Arc<RustConnection>);
impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> i32 {
        self.0.stream().as_raw_fd()
    }
}

#[derive(Clone, Copy)]
struct Atoms {
    utf8: Atom,
    property: Atom,
    incr: Atom,
    targets: Atom,
    clipboard: Atom,
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Self> {
        let atom =
            |name: &[u8]| -> Result<Atom> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        Ok(Self {
            utf8: atom(b"UTF8_STRING")?,
            property: atom(b"_TRAN_PRIMARY")?,
            incr: atom(b"INCR")?,
            targets: atom(b"TARGETS")?,
            clipboard: atom(b"CLIPBOARD")?,
        })
    }
}

struct Capture {
    ticket: Ticket,
    window: u32,
    owner: u32,
    origin: Origin,
    deadline: Instant,
    bytes: Vec<u8>,
    incremental: bool,
}

enum Command {
    Capture(Ticket),
    Cancel,
    Copy(String),
    Focus(u32),
}

pub(super) struct Device {
    conn: Arc<RustConnection>,
    root: u32,
    commands: mpsc::UnboundedSender<Command>,
    wake: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Device {
    pub const NAME: &'static str = "x11";
    pub fn new(proxy: EventLoopProxy<Event>, runtime: &Handle) -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("无法连接明确的 X11 会话")?;
        let root = connection.setup().roots[screen].root;
        let conn = Arc::new(connection);
        let atoms = Atoms::new(&conn)?;
        let owner = conn.generate_id()?;
        conn.create_window(
            0,
            owner,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
        conn.flush()?;
        let (commands, receiver) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let state = Worker {
            conn: conn.clone(),
            root,
            atoms,
            owner,
            clipboard: None,
            capture: None,
            proxy,
            wake: wake.clone(),
            receiver,
        };
        let task = runtime.spawn(async move {
            let proxy = state.proxy.clone();
            if let Err(error) = state.run().await {
                let _ = proxy.send_event(Event::Failed(error.to_string()));
            }
        });
        Ok(Self {
            conn,
            root,
            commands,
            wake,
            task,
        })
    }
    pub fn check() -> Result<()> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let pointer = conn.query_pointer(root)?.reply()?;
        if !pointer.same_screen {
            bail!("鼠标不属于当前 X11 screen")
        }
        println!("桌面：X11；连接与当前 screen 已确认；未请求 PRIMARY、未映射窗口。");
        Ok(())
    }
    pub fn origin(&self) -> Result<Origin> {
        let result = origin(&self.conn, self.root);
        self.wake.notify_one();
        result
    }
    pub fn capture(&mut self, ticket: Ticket) -> Result<()> {
        self.commands.send(Command::Capture(ticket))?;
        Ok(())
    }
    pub fn cancel(&mut self) {
        let _ = self.commands.send(Command::Cancel);
    }
    pub fn copy(&mut self, text: String, _: &Window) -> Result<()> {
        self.commands.send(Command::Copy(text))?;
        Ok(())
    }
    pub fn show(&self, window: &Window) -> Result<()> {
        window.set_visible(true);
        Ok(())
    }
    pub fn hide(&self, window: &Window) {
        window.set_visible(false);
    }
    pub fn focus(&self, window: &Window) -> Result<()> {
        self.commands.send(Command::Focus(window_id(window)?))?;
        Ok(())
    }
    pub fn owns(&self, source: u64, window: &Window) -> bool {
        window_id(window).is_ok_and(|id| id as u64 == source)
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn window_id(window: &Window) -> Result<u32> {
    match window.window_handle()?.as_raw() {
        RawWindowHandle::Xlib(handle) => Ok(handle.window as u32),
        RawWindowHandle::Xcb(handle) => Ok(handle.window.get()),
        _ => bail!("X11 前端收到非 X11 窗口，不切换显示服务"),
    }
}

fn origin(conn: &RustConnection, root: u32) -> Result<Origin> {
    let pointer = conn.query_pointer(root)?.reply()?;
    if !pointer.same_screen {
        bail!("鼠标不属于当前 X11 screen")
    }
    let source = conn.get_input_focus()?.reply()?.focus;
    Ok(Origin {
        source: source as u64,
        point: (pointer.root_x as f64, pointer.root_y as f64),
        logical: false,
    })
}

struct Worker {
    conn: Arc<RustConnection>,
    root: u32,
    atoms: Atoms,
    owner: u32,
    clipboard: Option<String>,
    capture: Option<Capture>,
    proxy: EventLoopProxy<Event>,
    wake: Arc<Notify>,
    receiver: mpsc::UnboundedReceiver<Command>,
}

impl Worker {
    async fn run(mut self) -> Result<()> {
        // SAFETY: Fd 持有 connection；注册期间 stream 的 fd 不替换、不关闭。
        let fd = unsafe { AsyncFd::register(Fd(self.conn.clone())) }
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        loop {
            while let Some(event) = self.conn.poll_for_event()? {
                self.event(event)?;
            }
            let deadline = self.capture.as_ref().map(|capture| capture.deadline);
            tokio::select! {
                command = self.receiver.recv() => match command {
                    Some(command) => self.command(command)?,
                    None => break,
                },
                ready = fd.readable() => { ready?.clear_ready(); },
                _ = self.wake.notified() => {},
                _ = async {
                    if let Some(deadline) = deadline { tokio::time::sleep_until(deadline.into()).await; }
                    else { std::future::pending::<()>().await; }
                } => self.complete(Err(Failure::CaptureTimeout))?,
            }
        }
        Ok(())
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Cancel => self.cancel()?,
            Command::Focus(window) => {
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, window, CURRENT_TIME)?;
            }
            Command::Copy(text) => {
                self.conn
                    .set_selection_owner(self.owner, self.atoms.clipboard, CURRENT_TIME)?
                    .check()?;
                if self
                    .conn
                    .get_selection_owner(self.atoms.clipboard)?
                    .reply()?
                    .owner
                    != self.owner
                {
                    bail!("无法取得 X11 剪贴板所有权")
                }
                self.clipboard = Some(text);
            }
            Command::Capture(ticket) => {
                self.cancel()?;
                let origin = origin(&self.conn, self.root)?;
                let owner = self
                    .conn
                    .get_selection_owner(AtomEnum::PRIMARY.into())?
                    .reply()?
                    .owner;
                if owner == NONE {
                    let _ = self.proxy.send_event(Event::Captured {
                        ticket,
                        origin,
                        text: Ok(String::new()),
                    });
                } else if origin.source <= 1 {
                    let _ = self.proxy.send_event(Event::Captured {
                        ticket,
                        origin,
                        text: Err(Failure::CaptureUnavailable),
                    });
                } else {
                    let window = self.conn.generate_id()?;
                    self.conn
                        .create_window(
                            0,
                            window,
                            self.root,
                            0,
                            0,
                            1,
                            1,
                            0,
                            WindowClass::INPUT_ONLY,
                            COPY_FROM_PARENT,
                            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                        )?
                        .check()?;
                    self.conn
                        .change_window_attributes(
                            origin.source as u32,
                            &ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::FOCUS_CHANGE | EventMask::STRUCTURE_NOTIFY),
                        )?
                        .check()?;
                    self.conn.convert_selection(
                        window,
                        AtomEnum::PRIMARY.into(),
                        self.atoms.utf8,
                        self.atoms.property,
                        CURRENT_TIME,
                    )?;
                    self.capture = Some(Capture {
                        ticket,
                        window,
                        owner,
                        origin,
                        deadline: Instant::now() + Duration::from_millis(800),
                        bytes: Vec::new(),
                        incremental: false,
                    });
                }
            }
        }
        self.conn.flush()?;
        Ok(())
    }
    fn cancel(&mut self) -> Result<()> {
        if let Some(capture) = self.capture.take() {
            self.conn.destroy_window(capture.window)?;
            self.conn.flush()?;
        }
        Ok(())
    }
    fn complete(&mut self, result: std::result::Result<String, Failure>) -> Result<()> {
        let Some(capture) = self.capture.take() else {
            return Ok(());
        };
        self.conn.destroy_window(capture.window)?;
        self.conn.flush()?;
        let _ = self.proxy.send_event(Event::Captured {
            ticket: capture.ticket,
            origin: capture.origin,
            text: result,
        });
        Ok(())
    }
    fn valid(&self) -> Result<bool> {
        let Some(capture) = &self.capture else {
            return Ok(false);
        };
        Ok(self
            .conn
            .get_selection_owner(AtomEnum::PRIMARY.into())?
            .reply()?
            .owner
            == capture.owner
            && self.conn.get_input_focus()?.reply()?.focus as u64 == capture.origin.source)
    }
    fn property(&mut self) -> Result<()> {
        if !self.valid()? {
            return self.complete(Ok(String::new()));
        }
        let capture = self.capture.as_mut().unwrap();
        let data = self
            .conn
            .get_property(
                true,
                capture.window,
                self.atoms.property,
                AtomEnum::ANY,
                0,
                (MAX_SELECTION_BYTES / 4 + 1) as u32,
            )?
            .reply()?;
        if data.bytes_after > 0 || capture.bytes.len() + data.value.len() > MAX_SELECTION_BYTES {
            return self.complete(Err(Failure::SelectionTooLarge));
        }
        if data.type_ == self.atoms.incr && !capture.incremental {
            if data.format != 32
                || data
                    .value32()
                    .and_then(|mut values| values.next())
                    .is_none_or(|len| len as usize > MAX_SELECTION_BYTES)
            {
                return self.complete(Err(Failure::SelectionTooLarge));
            }
            capture.incremental = true;
            self.conn.flush()?;
            return Ok(());
        }
        if data.type_ != self.atoms.utf8 || data.format != 8 {
            return self.complete(Err(Failure::InvalidText));
        }
        let done = !capture.incremental || data.value.is_empty();
        capture.bytes.extend(data.value);
        if done {
            let bytes = std::mem::take(&mut capture.bytes);
            let text = String::from_utf8(bytes).map_err(|_| Failure::InvalidText);
            self.complete(text)?;
        }
        self.conn.flush()?;
        Ok(())
    }
    fn event(&mut self, event: XEvent) -> Result<()> {
        match event {
            XEvent::SelectionNotify(event)
                if self
                    .capture
                    .as_ref()
                    .is_some_and(|capture| capture.window == event.requestor)
                    && event.selection == u32::from(AtomEnum::PRIMARY)
                    && event.target == self.atoms.utf8 =>
            {
                if event.property == NONE {
                    self.complete(Err(Failure::CaptureUnavailable))?;
                } else if event.property == self.atoms.property {
                    self.property()?;
                }
            }
            XEvent::PropertyNotify(event)
                if event.state == Property::NEW_VALUE
                    && event.atom == self.atoms.property
                    && self.capture.as_ref().is_some_and(|capture| {
                        capture.window == event.window && capture.incremental
                    }) =>
            {
                self.property()?
            }
            XEvent::DestroyNotify(_) => {
                if self.capture.is_some() {
                    self.complete(Ok(String::new()))?;
                }
                let _ = self.proxy.send_event(Event::SourceInvalidated);
            }
            XEvent::FocusOut(_) => {
                if self.capture.is_some() {
                    self.complete(Ok(String::new()))?;
                }
                let source = self.conn.get_input_focus()?.reply()?.focus;
                let _ = self.proxy.send_event(Event::Focus(source as u64));
            }
            XEvent::SelectionClear(event) if event.selection == self.atoms.clipboard => {
                self.clipboard = None
            }
            XEvent::SelectionRequest(event)
                if event.selection == self.atoms.clipboard && event.owner == self.owner =>
            {
                let mut property = NONE;
                if let Some(text) = &self.clipboard {
                    if event.property != NONE && event.target == self.atoms.targets {
                        self.conn.change_property32(
                            PropMode::REPLACE,
                            event.requestor,
                            event.property,
                            AtomEnum::ATOM,
                            &[self.atoms.targets, self.atoms.utf8],
                        )?;
                        property = event.property;
                    } else if event.property != NONE && event.target == self.atoms.utf8 {
                        self.conn.change_property8(
                            PropMode::REPLACE,
                            event.requestor,
                            event.property,
                            self.atoms.utf8,
                            text.as_bytes(),
                        )?;
                        property = event.property;
                    }
                }
                let reply = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: event.time,
                    requestor: event.requestor,
                    selection: event.selection,
                    target: event.target,
                    property,
                };
                self.conn
                    .send_event(false, event.requestor, EventMask::NO_EVENT, reply)?;
                self.conn.flush()?;
            }
            _ => {}
        }
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            let _ = self.conn.destroy_window(capture.window);
        }
        let _ = self.conn.destroy_window(self.owner);
        let _ = self.conn.flush();
    }
}
