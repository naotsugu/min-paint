use kurbo::{BezPath};
use xilem::winit::error::EventLoopError;
use xilem::{WidgetView, WindowOptions, Xilem};
use xilem::dpi::LogicalSize;
use xilem::view::{flex_row, label};
use xilem::winit::event_loop::EventLoop;

#[derive(Default, Clone)]
struct AppState {
    current_path: BezPath,
    lines: Vec<BezPath>,
    is_drawing: bool,
}

fn app_logic(data: &mut AppState) -> impl WidgetView<AppState> + use<> {
    flex_row(label("Color Picker").text_size(24.0),)
}

fn main() -> Result<(), EventLoopError> {
    let app_data = AppState::default();
    let min_window_size = LogicalSize::new(440., 300.);
    let window_options = WindowOptions::new("min-paint")
        .with_min_inner_size(min_window_size);
    let app = Xilem::new_simple(app_data, app_logic, window_options);
    app.run_in(EventLoop::with_user_event())
}

