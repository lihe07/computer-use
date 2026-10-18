mod keyboard_handler;
mod pointer_handler;

use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use egui::Rect;
use egui_software_backend::BufferMutRef;
use keyboard_handler::handle_key_press;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat,
    delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{ping::make_ping, LoopHandle},
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{
        wl_keyboard::WlKeyboard, wl_output, wl_pointer::WlPointer, wl_seat, wl_shm, wl_surface,
    },
    Connection, QueueHandle,
};

use crate::{
    egui_state::{self},
    App,
};

#[derive(Default)]
pub struct LayerShellOptions {
    pub layer: Option<Layer>,
    pub namespace: String,
    pub width: u32,
    pub height: u32,
    pub anchor: Option<Anchor>,
    pub keyboard_interactivity: Option<KeyboardInteractivity>,
    /// When true, the surface gets an empty input region, making it
    /// completely click-through (all pointer input passes to windows below).
    pub input_passthrough: bool,
}

pub(crate) struct WgpuLayerShellState {
    //event_loop: Arc<EventLoop<'static, Self>>,
    loop_handle: LoopHandle<'static, Self>,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    pub(crate) queue_handle: Arc<QueueHandle<Self>>,

    pub(crate) layer: LayerSurface,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,

    pub(crate) has_frame_callback: bool,
    is_configured: bool,

    pub(crate) exit: bool,

    pub(crate) egui_state: egui_state::State,
    pub(crate) draw_request: Arc<RwLock<Option<Instant>>>,

    pub(crate) shm: Shm,
    pub(crate) pool: SlotPool,

    /// Two shm buffers, alternated so we never rewrite one the compositor is
    /// still reading from.
    buffers: Vec<Buffer>,
    buffer_size: (i32, i32),
    frame: usize,
    /// What was painted into each buffer the last time it was used (i.e. two
    /// frames ago) — exactly the region that has to be cleared before reuse.
    buffer_damage: [Vec<IRect>; 2],
    /// What was painted last frame, i.e. what is on screen right now.
    prev_damage: Vec<IRect>,
    /// Scratch, reused every frame to keep the draw path allocation-free.
    damage: Vec<Rect>,
    frame_damage: Vec<IRect>,
    surface_damage: Vec<IRect>,

    /// Current output scale factor (integer buffer scale).
    scale: i32,
}

impl WgpuLayerShellState {
    pub(crate) fn new(loop_handle: LoopHandle<'static, Self>, options: LayerShellOptions) -> Self {
        let connection = Connection::connect_to_env().unwrap();
        let (global_list, event_queue) = registry_queue_init(&connection).unwrap();
        let queue_handle: Arc<QueueHandle<WgpuLayerShellState>> = Arc::new(event_queue.handle());

        WaylandSource::new(connection.clone(), event_queue)
            .insert(loop_handle.clone())
            .unwrap();

        let compositor_state = CompositorState::bind(&global_list, &queue_handle)
            .expect("wl_compositor not available");

        let wl_surface = compositor_state.create_surface(&queue_handle);

        let layer_shell =
            LayerShell::bind(&global_list, &queue_handle).expect("layer shell not available");
        let layer_surface = layer_shell.create_layer_surface(
            &queue_handle,
            wl_surface,
            options.layer.unwrap_or(Layer::Top),
            Some(options.namespace),
            None,
        );
        if let Some(anchor) = options.anchor {
            layer_surface.set_anchor(anchor);
        }
        if let Some(keyboard_interactivity) = options.keyboard_interactivity {
            layer_surface.set_keyboard_interactivity(keyboard_interactivity);
        }
        layer_surface.set_size(options.width, options.height);
        if options.input_passthrough {
            if let Ok(empty_region) =
                smithay_client_toolkit::compositor::Region::new(&compositor_state)
            {
                layer_surface
                    .wl_surface()
                    .set_input_region(Some(empty_region.wl_region()));
            }
        }
        layer_surface.commit();

        let shm = Shm::bind(&global_list, &queue_handle).expect("wl_shm not available");
        let pool = SlotPool::new(256 * 256 * 4, &shm).expect("Failed to create slot pool");

        let egui_context = egui::Context::default();

        let draw_request = Arc::new(RwLock::new(None));

        // Repaints can be requested from other threads; the ping wakes the event
        // loop so it re-evaluates its timeout instead of sleeping until the next
        // Wayland event.
        let (ping, ping_source) = make_ping().expect("could not create ping source");
        loop_handle
            .insert_source(ping_source, |_, _, _| {})
            .expect("could not insert ping source");

        egui_context.set_request_repaint_callback({
            let draw_request = Arc::clone(&draw_request);
            move |info| {
                let mut draw_request = draw_request.write().unwrap();
                *draw_request = Some(Instant::now() + info.delay);
                ping.ping();
            }
        });

        let egui_state = egui_state::State::new(egui_context);

        WgpuLayerShellState {
            loop_handle: loop_handle.clone(),
            registry_state: RegistryState::new(&global_list),
            seat_state: SeatState::new(&global_list, &queue_handle),
            output_state: OutputState::new(&global_list, &queue_handle),

            exit: false,
            layer: layer_surface,

            pointer: None,
            keyboard: None,

            has_frame_callback: false,
            is_configured: false,

            queue_handle,

            egui_state,
            draw_request,

            shm,
            pool,

            buffers: Vec::new(),
            buffer_size: (0, 0),
            frame: 0,
            buffer_damage: [Vec::new(), Vec::new()],
            prev_damage: Vec::new(),
            damage: Vec::new(),
            frame_damage: Vec::new(),
            surface_damage: Vec::new(),

            scale: 1,
        }
    }

    /// Apply a new output scale: the shm buffer is sized in physical pixels
    /// and `wl_surface.set_buffer_scale` tells the compositor the ratio.
    fn apply_scale(&mut self, factor: i32) {
        if factor >= 1 && factor != self.scale {
            self.scale = factor;
            self.layer.wl_surface().set_buffer_scale(factor);
            self.egui_state.set_scale(factor as f32);
            *self.draw_request.write().unwrap() = Some(Instant::now());
        }
    }

    //fn request_redraw(&self, )

    pub(crate) fn should_draw(&mut self) -> bool {
        if !self.has_frame_callback {
            return false;
        }

        if !self.egui_state.input().events.is_empty() {
            return true;
        }

        match *self.draw_request.read().unwrap() {
            Some(time) => time <= Instant::now(),
            None => false,
        }
    }

    pub(crate) fn get_timeout(&self) -> Option<Duration> {
        match *self.draw_request.read().unwrap() {
            Some(instant) => {
                if self.has_frame_callback {
                    Some(instant.duration_since(Instant::now()))
                } else {
                    None
                }
            }
            None => None,
        }
    }

    pub(crate) fn draw(&mut self, application: &mut dyn App) {
        *self.draw_request.write().unwrap() = None;
        self.has_frame_callback = false;

        let full_output = self
            .egui_state
            .process_events(|ctx| application.update(ctx));

        let (w, h) = self.egui_state.get_size();
        let stride = w * 4;
        if w <= 0 || h <= 0 {
            return;
        }

        // A resize invalidates both buffers, so nothing about their contents
        // can be assumed and the whole surface has to be redrawn.
        let mut contents_unknown = false;
        if self.buffer_size != (w, h) {
            self.buffer_size = (w, h);
            self.buffers.clear();
            self.buffer_damage = [Vec::new(), Vec::new()];
            self.prev_damage.clear();
            contents_unknown = true;
        }

        while self.buffers.len() < 2 {
            match self
                .pool
                .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
            {
                Ok((buffer, _)) => self.buffers.push(buffer),
                Err(err) => {
                    log::error!("could not allocate shm buffer: {err}");
                    return;
                }
            }
        }

        let index = self.frame % 2;
        self.frame = self.frame.wrapping_add(1);

        // If the compositor still holds this buffer, take a fresh one instead
        // of stalling on it; its contents are then unknown, hence a full clear.
        if self.pool.canvas(&self.buffers[index]).is_none() {
            match self
                .pool
                .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
            {
                Ok((buffer, _)) => self.buffers[index] = buffer,
                Err(err) => {
                    log::error!("could not allocate shm buffer: {err}");
                    return;
                }
            }
            contents_unknown = true;
        }

        let canvas = match self.pool.canvas(&self.buffers[index]) {
            Some(canvas) => canvas,
            None => return,
        };

        // Clear only what this buffer still holds from two frames ago.
        if contents_unknown {
            canvas.fill(0);
        } else {
            for rect in &self.buffer_damage[index] {
                clear_rect(canvas, w, stride, *rect);
            }
        }

        self.damage.clear();
        let buffer_ref =
            &mut BufferMutRef::new(bytemuck::cast_slice_mut(canvas), w as usize, h as usize);
        self.egui_state.draw(full_output, buffer_ref, &mut self.damage);

        self.frame_damage.clear();
        self.frame_damage
            .extend(self.damage.iter().filter_map(|r| IRect::clamped(*r, w, h)));
        coalesce(&mut self.frame_damage, MAX_DAMAGE_RECTS);

        self.buffers[index]
            .attach_to(self.layer.wl_surface())
            .expect("buffer attach");

        // Damage has to cover everything that differs from what is on screen:
        // what we just painted, plus what the previous frame painted and we
        // did not.
        if contents_unknown {
            self.layer.wl_surface().damage_buffer(0, 0, w, h);
        } else {
            self.surface_damage.clear();
            self.surface_damage.extend_from_slice(&self.frame_damage);
            self.surface_damage.extend_from_slice(&self.prev_damage);
            coalesce(&mut self.surface_damage, MAX_DAMAGE_RECTS);
            for rect in &self.surface_damage {
                self.layer
                    .wl_surface()
                    .damage_buffer(rect.x, rect.y, rect.w, rect.h);
            }
        }

        self.buffer_damage[index].clear();
        self.buffer_damage[index].extend_from_slice(&self.frame_damage);
        self.prev_damage.clear();
        self.prev_damage.extend_from_slice(&self.frame_damage);

        // layer size stays in logical coordinates; the buffer is scale x larger
        let (lw, lh) = self.egui_state.get_logical_size();
        self.layer.set_size(lw as u32, lh as u32);

        self.layer
            .wl_surface()
            .frame(&self.queue_handle, self.layer.wl_surface().clone());

        self.layer.wl_surface().commit();
    }
}

/// Upper bound on damage rects handed to the compositor. Past a handful the
/// per-rect bookkeeping costs more than the pixels saved.
const MAX_DAMAGE_RECTS: usize = 16;

/// An integer pixel rectangle in buffer coordinates.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct IRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl IRect {
    /// Round `rect` outwards and clip it to the buffer. Rounding out by a pixel
    /// matters: anti-aliased edges bleed past the geometric bounds.
    fn clamped(rect: Rect, width: i32, height: i32) -> Option<Self> {
        let x0 = (rect.min.x.floor() as i32 - 1).clamp(0, width);
        let y0 = (rect.min.y.floor() as i32 - 1).clamp(0, height);
        let x1 = (rect.max.x.ceil() as i32 + 1).clamp(0, width);
        let y1 = (rect.max.y.ceil() as i32 + 1).clamp(0, height);
        (x1 > x0 && y1 > y0).then_some(Self {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }

    fn area(self) -> i64 {
        self.w as i64 * self.h as i64
    }

    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            w: (self.x + self.w).max(other.x + other.w) - x,
            h: (self.y + self.h).max(other.y + other.h) - y,
        }
    }
}

/// Merge rects that overlap or nearly do, then keep merging the cheapest
/// neighbouring pair until at most `max` remain.
fn coalesce(rects: &mut Vec<IRect>, max: usize) {
    if rects.len() < 2 {
        return;
    }

    rects.sort_unstable_by_key(|r| (r.y, r.x));

    let mut merged: Vec<IRect> = Vec::with_capacity(rects.len());
    for rect in rects.drain(..) {
        match merged.last_mut() {
            // Merging is only worth it while the union stays close to the area
            // actually painted — otherwise we would damage a lot of untouched
            // pixels just to shorten the list.
            Some(last) if last.union(rect).area() <= last.area() + rect.area() => {
                *last = last.union(rect);
            }
            _ => merged.push(rect),
        }
    }

    while merged.len() > max {
        let i = (0..merged.len() - 1)
            .min_by_key(|&i| {
                merged[i].union(merged[i + 1]).area() - merged[i].area() - merged[i + 1].area()
            })
            .unwrap();
        merged[i] = merged[i].union(merged[i + 1]);
        merged.remove(i + 1);
    }

    *rects = merged;
}

/// Zero one rectangle of a BGRA buffer, row by row.
fn clear_rect(canvas: &mut [u8], width: i32, stride: i32, rect: IRect) {
    debug_assert!(rect.x + rect.w <= width);
    let _ = width;
    for row in rect.y..rect.y + rect.h {
        let start = row as usize * stride as usize + rect.x as usize * 4;
        let end = start + rect.w as usize * 4;
        canvas[start..end].fill(0);
    }
}

delegate_registry!(WgpuLayerShellState);
impl ProvidesRegistryState for WgpuLayerShellState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_output!(WgpuLayerShellState);
impl OutputHandler for WgpuLayerShellState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

delegate_compositor!(WgpuLayerShellState);
impl CompositorHandler for WgpuLayerShellState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        self.apply_scale(new_factor);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.has_frame_callback = true;
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if let Some(info) = self.output_state.info(output) {
            self.apply_scale(info.scale_factor);
        }
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

delegate_layer!(WgpuLayerShellState);
impl LayerShellHandler for WgpuLayerShellState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        log::debug!(
            "LayerShell configure: size=({}, {})",
            configure.new_size.0, configure.new_size.1
        );
        if !self.is_configured {
            self.is_configured = true;
            self.has_frame_callback = true;
            *self.draw_request.write().unwrap() = Some(Instant::now());
        }

        self.egui_state
            .set_size(configure.new_size.0, configure.new_size.1);
    }
}

delegate_seat!(WgpuLayerShellState);
impl SeatHandler for WgpuLayerShellState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer if self.pointer.is_none() => {
                let pointer = self
                    .seat_state
                    .get_pointer(qh, &seat)
                    .expect("Failed to create pointer");
                self.pointer = Some(pointer);
            }
            Capability::Keyboard if self.keyboard.is_none() => {
                self.keyboard = Some(
                    self.seat_state
                        .get_keyboard_with_repeat(
                            qh,
                            &seat,
                            None,
                            self.loop_handle.clone(),
                            Box::new(|state, _wl_kbd, event| {
                                handle_key_press(event, true, &mut state.egui_state.input());
                            }),
                        )
                        .expect("Failed to create keyboard"),
                );
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer if self.pointer.is_some() => {
                self.pointer.take().unwrap().release();
            }
            Capability::Keyboard if self.keyboard.is_some() => {
                self.keyboard.take().unwrap().release();
            }
            _ => {}
        }

        if capability == Capability::Pointer && self.pointer.is_some() {
            self.pointer.take().unwrap().release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

delegate_shm!(WgpuLayerShellState);
impl ShmHandler for WgpuLayerShellState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
