use std::sync::Arc;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::application::ApplicationHandler;
use winit::window::{Window, WindowId};
use winit::event::{WindowEvent};
use winit::dpi::LogicalSize;

struct AppState {
    window: Arc<Window>,
}

pub struct App {
    state: Option<AppState>,
    render_context: Option<vello::util::RenderContext>,
}

impl App {

    pub fn new() -> Self {
        Self {
            state: None,
            render_context: None,
        }
    }

    pub async fn run() {
        let event_loop = EventLoop::new()
            .expect("Failed to create event loop");
        let app = App::new();
        {
            let mut app = app;
            event_loop.run_app(&mut app).expect("Event loop error");
        }
    }

}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplicationHandler for App {

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {

        let window_attrs = Window::default_attributes()
            .with_title("app")
            .with_inner_size(LogicalSize::new(800, 600));

        let window = Arc::new(
            event_loop
                .create_window(window_attrs)
                .expect("Failed to create window"),
        );

        self.state = Some(AppState {
            window: window.clone(),
        });

        window.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = &mut self.state else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => { }
            WindowEvent::RedrawRequested => { }
            WindowEvent::CursorMoved { position, .. } => { }
            WindowEvent::MouseInput {state: btn_state, button, .. } => { }
            WindowEvent::MouseWheel { delta, .. } => { }
            WindowEvent::Touch(touch) => { }
            WindowEvent::KeyboardInput { event, .. } => { }
            WindowEvent::ModifiersChanged(_) => { }
            WindowEvent::DroppedFile(path) => { }
            _ => { }
        }
    }
}
