//! Headless smoke harness: `cargo run -p hearth-ui-rune --example smoke -- --list-scenes`,
//! `-- --snap out.png --scene ready --size 1320x880 --scale 2`, or `-- --tree tree.json`.

fn main() {
    rune::run(hearth_ui_rune::app::spec());
}
