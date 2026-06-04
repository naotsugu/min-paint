use std::rc::Rc;
use std::num::NonZeroU32;
use vello_common::pixmap::Pixmap;
use vello_cpu::{RenderContext, Resources};
use vello_cpu::color::palette::css::{BLUE};
use vello_cpu::kurbo::{Line, Circle, Rect, Shape};
use vello_cpu::kurbo::Point;
use winit::window::{Window, WindowId};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::event::{ElementState, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent};

enum RenderState {
    Active {
        window: Rc<Window>,
        surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    },
    Suspended,
}

struct App {
    render_state: RenderState,
    renderer: RenderContext,
    resources: Resources,
    pixmap: Pixmap,
    mouse_down: bool,
    last_cursor_position: Option<Point>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if matches!(self.render_state, RenderState::Active { .. }) {
            return;
        }
        let window_attrs = Window::default_attributes()
            .with_inner_size(winit::dpi::PhysicalSize::new(
                self.pixmap.width() as u32,
                self.pixmap.height() as u32,
            ))
            .with_resizable(true)
            .with_title("min-paint")
            .with_visible(true)
            .with_active(true);

        let window = Rc::new(event_loop.create_window(window_attrs).unwrap());
        let context = softbuffer::Context::new(window.clone()).unwrap();
        let surface = softbuffer::Surface::new(&context, window.clone()).unwrap();
        self.render_state = RenderState::Active { window, surface };
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let RenderState::Active { window, surface } = &mut self.render_state else {
            return;
        };
        if window.id() != window_id {
            return;
        }

        match event {

            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                let width = size.width.max(1);
                let height = size.height.max(1);
                surface.resize(NonZeroU32::new(width).unwrap(), NonZeroU32::new(height).unwrap()).unwrap();

                self.pixmap.resize(width as u16, height as u16);
                self.renderer = RenderContext::new_with(
                    width as u16,
                    height as u16,
                    *self.renderer.render_settings(),
                );
                window.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                self.renderer.render_to_pixmap(&mut self.resources, &mut self.pixmap);
                let mut buffer = surface.buffer_mut().unwrap();
                let pixmap_data = self.pixmap.data();
                for (buffer_pixel, pixel) in buffer.iter_mut().zip(pixmap_data.iter()) {
                    *buffer_pixel = u32::from_le_bytes([pixel.b, pixel.g, pixel.r, 0]);
                }
                buffer.present().unwrap();
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                self.mouse_down = state == ElementState::Pressed;
                if !self.mouse_down {
                    self.last_cursor_position = None;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let current_pos = Point { x: position.x, y: position.y };
                if self.mouse_down {
                    if let Some(last_pos) = self.last_cursor_position {
                        self.renderer.set_paint(BLUE);
                        self.renderer.stroke_path(&Line::new(last_pos, current_pos).to_path(1.0));
                        self.renderer.flush();
                        window.request_redraw();
                    }
                }
                self.last_cursor_position = Some(current_pos);
            }
            _ => {}
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.render_state = RenderState::Suspended;
    }
}

fn main() {

    let width = 600;
    let height = 400;
    let mut app = App {
        render_state: RenderState::Suspended,
        renderer: RenderContext::new(width, height),
        resources: Resources::new(),
        pixmap: Pixmap::new(width, height),
        mouse_down: false,
        last_cursor_position: None,
    };

    let event_loop = EventLoop::new().unwrap();
    event_loop
        .run_app(&mut app)
        .expect("Couldn't run event loop");

}
