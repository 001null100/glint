//! Glint — a small, pretty text editor.

mod app;
mod buffer;
mod cmd;
mod config;
mod doc;
mod draw;
mod editor;
mod files;
mod find;
mod gfx;
mod highlight;
mod md;
mod overlay;
mod pairs;
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
