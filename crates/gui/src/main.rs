//! recap: record your screens, get one link.
//!
//! Everything happens in the window. Pick your screens once, press Record,
//! press Stop, and the link is on the clipboard.
//!
//! Settings live in ~/.config/recap/config.toml.

mod ui;

fn main() -> anyhow::Result<()> {
    ui::run()
}
