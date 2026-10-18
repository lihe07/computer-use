use egui::{epaint::Primitive, Context, FullOutput, Rect};

use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender as Renderer};

pub struct State {
    context: egui::Context,
    input: egui::RawInput,
    renderer: Renderer,
    start_time: std::time::Instant,
    size: Option<Rect>,
    scale: f32,
}

impl State {
    pub fn new(context: egui::Context) -> Self {
        let input = egui::RawInput {
            focused: true,
            viewport_id: egui::ViewportId::ROOT,
            ..Default::default()
        };

        let renderer = Renderer::new(ColorFieldOrder::Bgra)
            .with_convert_tris_to_rects(true)
            .with_allow_raster_opt(true)
            .with_caching(false);

        // input
        //     .viewports
        //     .entry(egui::ViewportId::ROOT)
        //     .or_default()
        //     .native_pixels_per_point = Some(1.0);

        Self {
            context,
            input,
            renderer,
            start_time: std::time::Instant::now(),
            size: None,
            scale: 1.0,
        }
    }

    /// Set the output scale factor: layout stays in logical coordinates,
    /// but egui tessellates and the shm buffer is sized in physical pixels.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        self.input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(scale);
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        let screen_rect = egui::Rect {
            min: egui::Pos2 { x: 0f32, y: 0f32 },
            max: egui::Pos2 {
                x: width as f32,
                y: height as f32,
            },
        };
        self.size = Some(screen_rect);
        self.input.screen_rect = Some(screen_rect);
    }

    /// Buffer size in physical pixels (logical size x scale).
    pub(crate) fn get_size(&self) -> (i32, i32) {
        self.size
            .map(|r| {
                (
                    (r.width() * self.scale).ceil() as i32,
                    (r.height() * self.scale).ceil() as i32,
                )
            })
            .unwrap()
    }

    /// Surface size in logical coordinates.
    pub(crate) fn get_logical_size(&self) -> (i32, i32) {
        self.size
            .map(|r| (r.width().ceil() as i32, r.height().ceil() as i32))
            .unwrap()
    }

    pub(crate) fn input(&mut self) -> &mut egui::RawInput {
        &mut self.input
    }

    pub fn context(&self) -> &egui::Context {
        &self.context
    }

    pub fn modifiers(&self) -> egui::Modifiers {
        self.input.modifiers
    }

    pub fn push_event(&mut self, event: egui::Event) {
        self.input.events.push(event);
    }

    pub fn process_events(&mut self, run_ui: impl FnMut(&Context)) -> FullOutput {
        // TODO: maybe we need to take input for a certain window / surface?
        self.input.time = Some(self.start_time.elapsed().as_secs_f64());

        let raw_input = self.input.take();
        /* if (&raw_input.events).len() > 0 {
            dbg!(&raw_input.events);
        } */
        self.context.run(raw_input, run_ui)
    }

    /// Rasterize `full_output` into `buffer`, appending the physical-pixel
    /// bounds of everything painted to `damage`.
    ///
    /// The surface covers the whole screen but egui only ever paints a small
    /// part of it, so the caller uses these rects to clear and to tell the
    /// compositor what changed — a full-screen clear plus full-screen damage
    /// costs far more than the drawing itself.
    pub fn draw(&mut self, full_output: FullOutput, buffer_ref: &mut BufferMutRef, damage: &mut Vec<Rect>) {
        let pixels_per_point = full_output.pixels_per_point;
        let clipped_primitives = self.context.tessellate(full_output.shapes, pixels_per_point);

        for primitive in &clipped_primitives {
            let bounds = match &primitive.primitive {
                Primitive::Mesh(mesh) => mesh.calc_bounds(),
                Primitive::Callback(callback) => callback.rect,
            };
            let rect = bounds.intersect(primitive.clip_rect);
            if rect.is_positive() {
                damage.push(rect * pixels_per_point);
            }
        }

        self.renderer.render(
            buffer_ref,
            &clipped_primitives,
            &full_output.textures_delta,
            pixels_per_point,
        );
    }
}
