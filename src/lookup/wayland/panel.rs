//! 浮动面板及一次性定位面。绘制、输入与 surface 生命周期在这里闭合。

use super::State;
use crate::core::{
    Content, Display, DisplayIdentity, PANEL_HEADER, PANEL_HEIGHT, PANEL_WIDTH, Painter, Term,
    Theme, Ticket, Translation,
};
use anyhow::{Context, Result, bail};
use smithay_client_toolkit::{
    compositor::CompositorState,
    seat::{
        pointer::{BTN_LEFT, PointerEvent, PointerEventKind},
        relative_pointer::{RelativeMotionEvent, RelativePointerState},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface},
    },
    shm::{
        Shm,
        slot::{Buffer, SlotPool},
    },
};
use std::time::{Duration, Instant};
use wayland_client::{
    QueueHandle,
    globals::GlobalList,
    protocol::{wl_output, wl_pointer, wl_surface},
};
use wayland_protocols::wp::{
    relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1,
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

const NAMESPACE: &str = env!("CARGO_PKG_NAME");
const WIDTH: i32 = PANEL_WIDTH as i32;
const HEIGHT: i32 = PANEL_HEIGHT as i32;
const HEADER: f64 = PANEL_HEADER;
const MAP_TIMEOUT: Duration = Duration::from_millis(800);
const SAMPLE_TIMEOUT: Duration = Duration::from_millis(1050);

pub(super) enum Action {
    Pin,
    Theme,
    Close,
    Retry(DisplayIdentity),
    Copy(String),
}
pub(super) enum Event {
    Action(Action),
    Anchored {
        ticket: Ticket,
        output: wl_output::WlOutput,
        bounds: (u32, u32),
        point: (f64, f64),
    },
}

pub(super) fn logical_bounds(size: (i32, i32)) -> Option<(u32, u32)> {
    if size.0 > 0 && size.1 > 0 {
        Some((size.0 as u32, size.1 as u32))
    } else {
        None
    }
}

pub(super) fn output_local(
    point: (f64, f64),
    origin: (i32, i32),
    bounds: (u32, u32),
) -> Option<(f64, f64)> {
    let local = (point.0 - origin.0 as f64, point.1 - origin.1 as f64);
    ((0.0..bounds.0 as f64).contains(&local.0) && (0.0..bounds.1 as f64).contains(&local.1))
        .then_some(local)
}

fn panel_fits(bounds: (u32, u32)) -> bool {
    bounds.0 >= WIDTH as u32
        && bounds.1 >= HEIGHT as u32
        && bounds.0 <= i32::MAX as u32
        && bounds.1 <= i32::MAX as u32
}

fn clamp_position(position: (f64, f64), bounds: (u32, u32)) -> (i32, i32) {
    let x = (position.0.round() as i32).clamp(0, (bounds.0 as i32 - WIDTH).max(0));
    let y = (position.1.round() as i32).clamp(0, (bounds.1 as i32 - HEIGHT).max(0));
    (x, y)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    view: u64,
    frame: u64,
    display: Option<DisplayIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Pin,
    Title,
    Theme,
    Close,
    Content,
}

fn hit(point: (f64, f64)) -> Option<Hit> {
    if !(0.0..WIDTH as f64).contains(&point.0) || !(0.0..HEIGHT as f64).contains(&point.1) {
        return None;
    }
    Some(if point.1 >= HEADER {
        Hit::Content
    } else if point.0 < 55.0 {
        Hit::Pin
    } else if point.0 > 230.0 {
        Hit::Close
    } else if point.0 > 175.0 {
        Hit::Theme
    } else {
        Hit::Title
    })
}

struct Press {
    identity: Identity,
    target: Hit,
    point: (f64, f64),
    origin: (i32, i32),
    delta: (f64, f64),
    moved: bool,
}

#[derive(Default)]
struct Interaction {
    pressed: Option<Press>,
    last_click: Option<(Identity, (f64, f64), u32)>,
}

impl Interaction {
    fn press(&mut self, identity: Identity, point: (f64, f64), origin: (i32, i32)) -> bool {
        let Some(target) = hit(point) else {
            self.pressed = None;
            return false;
        };
        if target != Hit::Content {
            self.last_click = None;
        }
        self.pressed = Some(Press {
            identity,
            target,
            point,
            origin,
            delta: (0.0, 0.0),
            moved: false,
        });
        target == Hit::Title
    }

    fn motion(&mut self, identity: Identity, point: (f64, f64)) {
        if let Some(press) = &mut self.pressed {
            if press.identity != identity {
                self.pressed = None;
            } else if press.target != Hit::Title && distant(press.point, point, 3.0) {
                // 曾经移出点击阈值就不是点击，移回原位也不能重新变成固定意图。
                press.moved = true;
                self.last_click = None;
            }
        }
    }

    fn drag(&mut self, delta: (f64, f64), bounds: (u32, u32)) -> Option<(Identity, (i32, i32))> {
        let press = self.pressed.as_mut()?;
        if press.target != Hit::Title || !delta.0.is_finite() || !delta.1.is_finite() {
            return None;
        }
        press.delta.0 += delta.0;
        press.delta.1 += delta.1;
        press.moved |= distant((0.0, 0.0), press.delta, 3.0);
        if !press.moved {
            return None;
        }
        self.last_click = None;
        Some((
            press.identity,
            clamp_position(
                (
                    press.origin.0 as f64 + press.delta.0,
                    press.origin.1 as f64 + press.delta.1,
                ),
                bounds,
            ),
        ))
    }

    fn release(&mut self, identity: Identity, point: (f64, f64)) -> bool {
        self.motion(identity, point);
        let Some(press) = self.pressed.take() else {
            return false;
        };
        press.identity == identity && !press.moved && hit(point) == Some(press.target)
    }

    fn double_click(&mut self, identity: Identity, point: (f64, f64), time: u32) -> bool {
        if self.last_click.is_some_and(|(previous, position, at)| {
            previous == identity && time.wrapping_sub(at) <= 500 && !distant(position, point, 4.0)
        }) {
            self.last_click = None;
            true
        } else {
            self.last_click = Some((identity, point, time));
            false
        }
    }
}

fn distant(first: (f64, f64), second: (f64, f64), threshold: f64) -> bool {
    (first.0 - second.0).hypot(first.1 - second.1) > threshold
}

struct Painted {
    identity: Identity,
    height: f64,
    terms: Vec<Term>,
}
enum Mapping {
    Configure(Instant),
    FirstFrame(Instant),
    Visible,
}

struct PanelState {
    id: u64,
    ticket: Ticket,
    bounds: (u32, u32),
    position: (i32, i32),
    scale: i32,
    scroll: f64,
    mapping: Mapping,
    dirty: bool,
    frame: u64,
    painted: Option<Painted>,
    input: Interaction,
}

impl PanelState {
    fn new(id: u64, ticket: Ticket, bounds: (u32, u32), point: (f64, f64)) -> Result<Self> {
        if !panel_fits(bounds) || !point.0.is_finite() || !point.1.is_finite() {
            bail!("目标输出放不下 256×100 面板或定位无效");
        }
        Ok(Self {
            id,
            ticket,
            bounds,
            position: clamp_position(point, bounds),
            scale: 1,
            scroll: 0.0,
            mapping: Mapping::Configure(Instant::now() + MAP_TIMEOUT),
            dirty: false,
            frame: 0,
            painted: None,
            input: Interaction::default(),
        })
    }

    fn deadline(&self) -> Option<Instant> {
        match self.mapping {
            Mapping::Configure(deadline) | Mapping::FirstFrame(deadline) => Some(deadline),
            Mapping::Visible => None,
        }
    }

    fn configure(&mut self) -> bool {
        self.dirty = true;
        if let Mapping::Configure(deadline) = self.mapping {
            self.mapping = Mapping::FirstFrame(deadline);
            true
        } else {
            false
        }
    }

    fn presented(&mut self, now: Instant) -> bool {
        if self.painted.is_some()
            && matches!(self.mapping, Mapping::FirstFrame(deadline) if now < deadline)
        {
            self.mapping = Mapping::Visible;
            true
        } else {
            false
        }
    }

    fn commit(&mut self, display: Option<DisplayIdentity>, height: f64, terms: Vec<Term>) {
        self.frame += 1;
        self.painted = Some(Painted {
            identity: Identity {
                view: self.id,
                frame: self.frame,
                display,
            },
            height,
            terms,
        });
        self.dirty = false;
    }

    fn identity(&self) -> Option<Identity> {
        if matches!(self.mapping, Mapping::Visible) {
            self.painted.as_ref().map(|paint| paint.identity)
        } else {
            None
        }
    }

    fn redraw(&mut self) {
        self.input = Interaction::default();
        self.dirty = true;
    }

    fn reticket(&mut self, ticket: Ticket) {
        if self.ticket != ticket {
            self.ticket = ticket;
            self.scroll = 0.0;
        }
        self.redraw();
    }

    fn resize(&mut self, bounds: (u32, u32)) -> Result<()> {
        if !panel_fits(bounds) {
            bail!("当前输出放不下面板；不裁切展示");
        }
        self.bounds = bounds;
        self.position = clamp_position((self.position.0 as f64, self.position.1 as f64), bounds);
        self.redraw();
        Ok(())
    }

    fn scale(&mut self, scale: i32) -> Result<()> {
        if !(1..=8).contains(&scale) {
            bail!("输出缩放超出面板支持范围");
        }
        if self.scale != scale {
            self.scale = scale;
            self.redraw();
        }
        Ok(())
    }

    fn scroll(&mut self, delta: f64) {
        let height = self.painted.as_ref().map_or(0.0, |paint| paint.height);
        let max = (height - (HEIGHT as f64 - HEADER - 8.0)).max(0.0);
        let next = (self.scroll + delta).clamp(0.0, max);
        if next != self.scroll {
            self.scroll = next;
            self.redraw();
        }
    }

    fn activate(
        &mut self,
        identity: Identity,
        point: (f64, f64),
        time: u32,
        content: Option<&Content>,
    ) -> Option<Action> {
        match hit(point)? {
            Hit::Pin => return Some(Action::Pin),
            Hit::Theme => return Some(Action::Theme),
            Hit::Close => return Some(Action::Close),
            Hit::Title => return None,
            Hit::Content => {}
        }
        let display = identity.display?;
        match content? {
            Content::Failed { retry: Some(_), .. } => Some(Action::Retry(display)),
            Content::Ready(Translation::Sentence(text)) => self
                .input
                .double_click(identity, point, time)
                .then(|| Action::Copy(text.clone())),
            Content::Ready(Translation::Dictionary(_)) => self
                .painted
                .as_ref()?
                .terms
                .iter()
                .find(|term| point.1 >= term.top && point.1 < term.bottom)
                .map(|term| Action::Copy(term.text.clone())),
            _ => None,
        }
    }
}

struct Surface {
    layer: LayerSurface,
    viewport: WpViewport,
    pool: SlotPool,
    buffers: Vec<(Buffer, i32, i32)>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.viewport.destroy();
    }
}

impl Surface {
    fn prepare(&mut self, width: i32, height: i32) -> Result<Option<usize>> {
        self.buffers.retain(|(_, w, h)| *w == width && *h == height);
        if let Some(index) = self
            .buffers
            .iter()
            .position(|(buffer, _, _)| self.pool.canvas(buffer).is_some())
        {
            return Ok(Some(index));
        }
        if self.buffers.len() >= 2 {
            return Ok(None);
        }
        let (buffer, _) = self.pool.create_buffer(
            width,
            height,
            width * 4,
            wayland_client::protocol::wl_shm::Format::Argb8888,
        )?;
        self.buffers.push((buffer, width, height));
        Ok(Some(self.buffers.len() - 1))
    }

    fn commit(
        &mut self,
        index: usize,
        pixels: &[u32],
        destination: (i32, i32),
        opaque: bool,
    ) -> Result<()> {
        let canvas = self
            .pool
            .canvas(&self.buffers[index].0)
            .context("绘制缓冲区不可用")?;
        if canvas.len() != pixels.len() * 4 {
            bail!("共享绘制帧与当前缓冲区尺寸不符");
        }
        for (target, pixel) in canvas.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
            target.copy_from_slice(
                &(if opaque { *pixel | 0xff000000 } else { *pixel }).to_ne_bytes(),
            );
        }
        self.viewport.set_destination(destination.0, destination.1);
        let (_, width, height) = &self.buffers[index];
        self.layer.wl_surface().damage_buffer(0, 0, *width, *height);
        self.buffers[index].0.attach_to(self.layer.wl_surface())?;
        self.layer.commit();
        Ok(())
    }
}

struct Window {
    surface: Surface,
    output: wl_output::WlOutput,
    state: PanelState,
}
struct Probe {
    surface: Surface,
    output: wl_output::WlOutput,
    bounds: (u32, u32),
    dirty: bool,
}
enum Scene {
    Hidden,
    Sampling {
        ticket: Ticket,
        probes: Vec<Probe>,
        deadline: Instant,
    },
    Panel(Box<Window>),
}

pub(super) struct Presenter {
    compositor: CompositorState,
    layers: LayerShell,
    viewporter: WpViewporter,
    shm: Shm,
    relative: RelativePointerState,
    qh: QueueHandle<State>,
    next: u64,
    scene: Scene,
    drag: Option<ZwpRelativePointerV1>,
    keyboard_focus: Option<wl_surface::WlSurface>,
    renderer: Box<dyn Painter>,
}

impl Presenter {
    pub(super) fn bind(
        globals: &GlobalList,
        qh: &QueueHandle<State>,
        renderer: Box<dyn Painter>,
    ) -> Result<Self> {
        Ok(Self {
            compositor: CompositorState::bind(globals, qh)?,
            layers: LayerShell::bind(globals, qh)?,
            viewporter: globals
                .bind(qh, 1..=1, ())
                .context("桌面未提供 viewporter")?,
            shm: Shm::bind(globals, qh)?,
            relative: RelativePointerState::bind(globals, qh),
            qh: qh.clone(),
            next: 0,
            scene: Scene::Hidden,
            drag: None,
            keyboard_focus: None,
            renderer,
        })
    }

    fn surface(&self, output: &wl_output::WlOutput) -> Result<Surface> {
        let surface = self.compositor.create_surface(&self.qh);
        let viewport = self.viewporter.get_viewport(&surface, &self.qh, ());
        let layer = self.layers.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some(NAMESPACE),
            Some(output),
        );
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        Ok(Surface {
            layer,
            viewport,
            pool: SlotPool::new(4, &self.shm)?,
            buffers: Vec::new(),
        })
    }

    pub(super) fn shm(&mut self) -> &mut Shm {
        &mut self.shm
    }
    pub(super) fn focused(&self) -> bool {
        self.keyboard_focus.is_some()
    }
    pub(super) fn positioning(&self) -> bool {
        match &self.scene {
            Scene::Sampling { .. } => true,
            Scene::Panel(window) => window.state.deadline().is_some(),
            Scene::Hidden => false,
        }
    }
    pub(super) fn contains_ticket(&self, ticket: Ticket) -> bool {
        matches!(&self.scene, Scene::Panel(window) if window.state.ticket == ticket)
    }
    pub(super) fn contains_output(&self, output: &wl_output::WlOutput) -> bool {
        match &self.scene {
            Scene::Panel(window) => &window.output == output,
            Scene::Sampling { probes, .. } => probes.iter().any(|probe| &probe.output == output),
            Scene::Hidden => false,
        }
    }
    pub(super) fn painted(&self) -> Option<DisplayIdentity> {
        match &self.scene {
            Scene::Panel(window) => window.state.identity()?.display,
            _ => None,
        }
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        match &self.scene {
            Scene::Sampling { deadline, .. } => Some(*deadline),
            Scene::Panel(window) => window.state.deadline(),
            Scene::Hidden => None,
        }
    }

    pub(super) fn clear_input(&mut self) {
        if let Some(drag) = self.drag.take() {
            drag.destroy();
        }
        if let Scene::Panel(window) = &mut self.scene {
            window.state.input = Interaction::default();
        }
    }

    pub(super) fn hide(&mut self) {
        self.clear_input();
        self.scene = Scene::Hidden;
        self.keyboard_focus = None;
        self.renderer.reset();
    }
    pub(super) fn cancel_positioning(&mut self) {
        if self.positioning() {
            self.hide();
        } else {
            self.clear_input();
        }
    }
    pub(super) fn redraw(&mut self) {
        self.clear_input();
        if let Scene::Panel(window) = &mut self.scene {
            window.state.redraw();
        }
    }
    pub(super) fn reuse(&mut self, ticket: Ticket) {
        self.clear_input();
        if let Scene::Panel(window) = &mut self.scene {
            window.state.reticket(ticket);
        }
    }

    pub(super) fn sample(
        &mut self,
        ticket: Ticket,
        outputs: Vec<(wl_output::WlOutput, (u32, u32))>,
    ) -> Result<()> {
        if outputs.is_empty() {
            bail!("没有可用的 Wayland 输出");
        }
        self.hide();
        let mut probes = Vec::new();
        for (output, bounds) in outputs {
            let surface = self.surface(&output)?;
            surface
                .layer
                .set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            surface.layer.set_size(bounds.0, bounds.1);
            surface.layer.commit();
            probes.push(Probe {
                surface,
                output,
                bounds,
                dirty: false,
            });
        }
        self.scene = Scene::Sampling {
            ticket,
            probes,
            deadline: Instant::now() + SAMPLE_TIMEOUT,
        };
        Ok(())
    }

    pub(super) fn show(
        &mut self,
        ticket: Ticket,
        output: wl_output::WlOutput,
        bounds: (u32, u32),
        point: (f64, f64),
    ) -> Result<()> {
        self.next += 1;
        let position = self
            .renderer
            .place(point, (0.0, 0.0, bounds.0 as f64, bounds.1 as f64))
            .context("输出放不下面板或鼠标不属于该输出")?;
        let state = PanelState::new(self.next, ticket, bounds, position)?;
        self.hide();
        let surface = self.surface(&output)?;
        surface.layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        surface
            .layer
            .set_margin(state.position.1, 0, 0, state.position.0);
        surface.layer.set_size(WIDTH as u32, HEIGHT as u32);
        let region = self.compositor.wl_compositor().create_region(&self.qh, ());
        surface.layer.wl_surface().set_input_region(Some(&region));
        region.destroy();
        surface.layer.commit();
        self.scene = Scene::Panel(Box::new(Window {
            surface,
            output,
            state,
        }));
        Ok(())
    }

    pub(super) fn draw(
        &mut self,
        display: Option<&Display>,
        pinned: bool,
        theme: Theme,
    ) -> Result<()> {
        match &mut self.scene {
            Scene::Panel(window)
                if window.state.dirty && !matches!(window.state.mapping, Mapping::Configure(_)) =>
            {
                let scale = window.state.scale;
                let Some(index) = window.surface.prepare(WIDTH * scale, HEIGHT * scale)? else {
                    return Ok(());
                };
                let Some(display) = display.filter(|display| display.ticket == window.state.ticket)
                else {
                    return Ok(());
                };
                let view = self.renderer.view(display, pinned, theme);
                let image = self.renderer.render(
                    &view,
                    ((WIDTH * scale) as u32, (HEIGHT * scale) as u32),
                    scale as f64,
                    window.state.scroll,
                );
                window
                    .surface
                    .commit(index, &image.pixels, (WIDTH, HEIGHT), true)?;
                window
                    .state
                    .commit(Some(display.identity()), image.height, image.terms);
            }
            Scene::Sampling { probes, .. } => {
                for probe in probes.iter_mut().filter(|probe| probe.dirty) {
                    if let Some(index) = probe.surface.prepare(1, 1)? {
                        probe.surface.commit(
                            index,
                            &[0],
                            (probe.bounds.0 as i32, probe.bounds.1 as i32),
                            false,
                        )?;
                        probe.dirty = false;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn configure(&mut self, layer: &LayerSurface) {
        match &mut self.scene {
            Scene::Panel(window) if &window.surface.layer == layer => {
                if window.state.configure() {
                    window
                        .surface
                        .layer
                        .wl_surface()
                        .frame(&self.qh, window.surface.layer.wl_surface().clone());
                    window.surface.layer.wl_surface().set_input_region(None);
                }
            }
            Scene::Sampling { probes, .. } => {
                if let Some(probe) = probes
                    .iter_mut()
                    .find(|probe| &probe.surface.layer == layer)
                {
                    probe.dirty = true;
                }
            }
            _ => {}
        }
    }

    pub(super) fn frame(&mut self, surface: &wl_surface::WlSurface) {
        if let Scene::Panel(window) = &mut self.scene
            && window.surface.layer.wl_surface() == surface
            && window.state.presented(Instant::now())
        {
            window
                .surface
                .layer
                .set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
            window.surface.layer.commit();
        }
    }

    pub(super) fn closed(&self, layer: &LayerSurface) -> bool {
        match &self.scene {
            Scene::Panel(window) => &window.surface.layer == layer,
            Scene::Sampling { probes, .. } => {
                probes.iter().any(|probe| &probe.surface.layer == layer)
            }
            Scene::Hidden => false,
        }
    }

    pub(super) fn resize(
        &mut self,
        output: &wl_output::WlOutput,
        bounds: (u32, u32),
    ) -> Result<()> {
        self.clear_input();
        if let Scene::Panel(window) = &mut self.scene
            && &window.output == output
        {
            window.state.resize(bounds)?;
            window
                .surface
                .layer
                .set_margin(window.state.position.1, 0, 0, window.state.position.0);
            window.surface.layer.commit();
        }
        Ok(())
    }

    pub(super) fn scale(&mut self, surface: &wl_surface::WlSurface, scale: i32) -> Result<()> {
        self.clear_input();
        if let Scene::Panel(window) = &mut self.scene
            && window.surface.layer.wl_surface() == surface
        {
            window.state.scale(scale)?;
        }
        Ok(())
    }

    pub(super) fn keyboard_enter(&mut self, surface: &wl_surface::WlSurface) -> bool {
        if let Scene::Panel(window) = &self.scene
            && window.surface.layer.wl_surface() == surface
        {
            self.keyboard_focus = Some(surface.clone());
            true
        } else {
            false
        }
    }

    pub(super) fn keyboard_leave(&mut self, surface: &wl_surface::WlSurface) -> bool {
        if self.keyboard_focus.as_ref() == Some(surface) {
            self.keyboard_focus = None;
            true
        } else {
            false
        }
    }

    pub(super) fn pointer(
        &mut self,
        pointer: &wl_pointer::WlPointer,
        event: &PointerEvent,
        content: Option<&Content>,
    ) -> Option<Event> {
        if let Scene::Sampling { ticket, probes, .. } = &self.scene {
            if matches!(event.kind, PointerEventKind::Enter { .. })
                && let Some(probe) = probes
                    .iter()
                    .find(|probe| probe.surface.layer.wl_surface() == &event.surface)
            {
                return Some(Event::Anchored {
                    ticket: *ticket,
                    output: probe.output.clone(),
                    bounds: probe.bounds,
                    point: event.position,
                });
            }
            return None;
        }
        let Scene::Panel(window) = &mut self.scene else {
            return None;
        };
        if window.surface.layer.wl_surface() != &event.surface {
            return None;
        }
        let identity = window.state.identity()?;
        match event.kind {
            PointerEventKind::Press {
                button: BTN_LEFT, ..
            } => {
                if let Some(drag) = self.drag.take() {
                    drag.destroy();
                }
                if window
                    .state
                    .input
                    .press(identity, event.position, window.state.position)
                {
                    match self.relative.get_relative_pointer(pointer, &self.qh) {
                        Ok(drag) => self.drag = Some(drag),
                        Err(_) => {
                            self.clear_input();
                            eprintln!("Glosslet：桌面未提供 relative-pointer，无法安全拖动面板");
                        }
                    }
                }
            }
            PointerEventKind::Motion { .. } => window.state.input.motion(identity, event.position),
            PointerEventKind::Release {
                button: BTN_LEFT,
                time,
                ..
            } => {
                let clicked = window.state.input.release(identity, event.position);
                if let Some(drag) = self.drag.take() {
                    drag.destroy();
                }
                if clicked {
                    return window
                        .state
                        .activate(identity, event.position, time, content)
                        .map(Event::Action);
                }
            }
            PointerEventKind::Axis { vertical, .. } => {
                self.clear_input();
                if let Scene::Panel(window) = &mut self.scene {
                    window.state.scroll(vertical.absolute);
                }
            }
            PointerEventKind::Leave { .. } | PointerEventKind::Press { .. } => self.clear_input(),
            _ => {}
        }
        None
    }

    pub(super) fn relative_motion(
        &mut self,
        relative: &ZwpRelativePointerV1,
        event: RelativeMotionEvent,
    ) {
        if self.drag.as_ref() != Some(relative) {
            return;
        }
        let Scene::Panel(window) = &mut self.scene else {
            return;
        };
        let Some(identity) = window.state.identity() else {
            return;
        };
        if window
            .state
            .input
            .pressed
            .as_ref()
            .is_some_and(|press| press.identity != identity)
        {
            self.clear_input();
            return;
        }
        if let Some((_, position)) = window.state.input.drag(event.delta, window.state.bounds)
            && position != window.state.position
        {
            window.state.position = position;
            window
                .surface
                .layer
                .set_margin(position.1, 0, 0, position.0);
            window.surface.layer.commit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::DictionaryEntry;
    use crate::lookup::session::Session;

    fn display() -> DisplayIdentity {
        let mut session = Session::default();
        session.probe_panel();
        session.display().unwrap().identity()
    }

    fn visible() -> PanelState {
        let mut state = PanelState::new(1, Ticket(1), (1920, 1080), (500.0, 500.0)).unwrap();
        state.configure();
        state.commit(Some(display()), 100.0, vec![]);
        assert!(state.presented(Instant::now()));
        state
    }

    #[test]
    fn input_needs_configuration_commit_and_one_timely_frame_callback() {
        let mut state = PanelState::new(1, Ticket(1), (1920, 1080), (0.0, 0.0)).unwrap();
        assert!(!state.presented(Instant::now()));
        assert!(state.configure());
        assert!(!state.configure());
        assert!(!state.presented(Instant::now()));
        state.commit(Some(display()), 100.0, vec![]);
        assert!(state.identity().is_none());
        assert!(state.presented(Instant::now()));
        assert!(!state.presented(Instant::now()));
        assert!(state.identity().is_some());
        let mut late = PanelState::new(2, Ticket(2), (1920, 1080), (0.0, 0.0)).unwrap();
        late.configure();
        late.commit(Some(display()), 100.0, vec![]);
        assert!(!late.presented(late.deadline().unwrap()));
        assert!(late.identity().is_none());
    }

    #[test]
    fn repaint_resize_and_reticket_cannot_complete_old_gestures() {
        let mut state = visible();
        let old = state.identity().unwrap();
        state.input.press(old, (10.0, 10.0), state.position);
        state.commit(Some(display()), 100.0, vec![]);
        assert!(!state.input.release(state.identity().unwrap(), (10.0, 10.0)));
        let current = state.identity().unwrap();
        state.input.press(current, (10.0, 10.0), state.position);
        state.resize((1280, 720)).unwrap();
        assert!(!state.input.release(current, (10.0, 10.0)));
        state.input.press(current, (10.0, 10.0), state.position);
        state.reticket(Ticket(2));
        assert!(!state.input.release(current, (10.0, 10.0)));
    }

    #[test]
    fn moving_away_and_back_or_releasing_on_another_control_is_not_a_click() {
        let mut state = visible();
        let identity = state.identity().unwrap();
        state.input.press(identity, (10.0, 10.0), state.position);
        state.input.motion(identity, (30.0, 10.0));
        state.input.motion(identity, (10.0, 10.0));
        assert!(!state.input.release(identity, (10.0, 10.0)));
        state.input.press(identity, (54.0, 10.0), state.position);
        assert!(!state.input.release(identity, (56.0, 10.0)));
    }

    #[test]
    fn dragging_is_relative_clamped_and_never_becomes_a_toolbar_click() {
        let mut state = visible();
        let identity = state.identity().unwrap();
        assert!(state.input.press(identity, (100.0, 10.0), (1800, 1000)));
        assert_eq!(
            state.input.drag((100.0, 100.0), state.bounds),
            Some((identity, (1664, 980)))
        );
        assert!(state.input.drag((f64::NAN, 0.0), state.bounds).is_none());
        assert!(!state.input.release(identity, (100.0, 10.0)));
        assert!(!state.input.press(identity, (10.0, 10.0), state.position));
        assert!(state.input.drag((10.0, 10.0), state.bounds).is_none());
    }

    #[test]
    fn sentence_copy_requires_a_nearby_double_click_on_current_content() {
        let mut state = visible();
        let identity = state.identity().unwrap();
        let content = Content::Ready(Translation::Sentence("result".into()));
        assert!(
            state
                .activate(identity, (20.0, 40.0), u32::MAX - 100, None)
                .is_none()
        );
        assert!(
            state
                .activate(identity, (20.0, 40.0), u32::MAX - 100, Some(&content))
                .is_none()
        );
        assert!(
            matches!(state.activate(identity, (21.0, 41.0), 200, Some(&content)), Some(Action::Copy(text)) if text == "result")
        );
        state.redraw();
        assert!(
            state
                .activate(identity, (20.0, 40.0), 300, Some(&content))
                .is_none()
        );
    }

    #[test]
    fn dictionary_copy_uses_visible_painted_term_boxes_and_loading_cannot_copy() {
        let mut state = visible();
        let content = Content::Ready(Translation::Dictionary(vec![DictionaryEntry {
            pos: "noun".into(),
            terms: vec!["第一条释义".into(), "second meaning".into()],
        }]));
        let height = 100.0;
        let terms = vec![Term {
            top: HEADER + 6.0,
            bottom: HEADER + 23.0,
            text: "第一条释义".into(),
        }];
        let point = (20.0, (terms[0].top + terms[0].bottom) / 2.0);
        state.commit(Some(display()), height, terms);
        let identity = state.identity().unwrap();
        assert!(
            matches!(state.activate(identity, point, 100, Some(&content)), Some(Action::Copy(text)) if text == "第一条释义")
        );
        assert!(
            state
                .activate(identity, point, 100, Some(&Content::Loading))
                .is_none()
        );
    }

    #[test]
    fn fixed_size_placement_uses_logical_geometry_and_space_above_screen_edges() {
        assert!(PanelState::new(1, Ticket(1), (255, 100), (0.0, 0.0)).is_err());
        assert!(logical_bounds((-1, 100)).is_none());
        assert_eq!(
            output_local((-1800.0, 300.0), (-1920, 0), (1920, 1080)),
            Some((120.0, 300.0))
        );
        assert_eq!(clamp_position((1840.0, 1070.0), (1920, 1080)), (1664, 980));
        assert!(output_local((f64::NAN, 0.0), (0, 0), (1920, 1080)).is_none());
        assert!(PanelState::new(1, Ticket(1), (u32::MAX, 100), (0.0, 0.0)).is_err());
        assert!(PanelState::new(1, Ticket(1), (1920, 1080), (f64::NAN, 0.0)).is_err());
    }
}
