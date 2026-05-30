use crate::app::App;

mod app;

fn main() {
    pollster::block_on(App::run());
}
