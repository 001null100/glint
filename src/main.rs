//! Glint — a small, pretty text editor.

mod app;
mod buffer;
mod cmd;
mod config;
mod doc;
mod draw;
mod editor;
mod find;
mod gfx;
mod highlight;
mod overlay;
mod pane;
mod platform;
mod runner;
mod scene;
mod session;
mod text;
mod theme;
mod ui;
mod util;
mod wrap;

fn main() {
    runner::main();
}
