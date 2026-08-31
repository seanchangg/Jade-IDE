//! Dev tool: run the Visualize script gate over a file.
//!
//!     cargo run -p jade-build --example validate_scene -- scene.py
//!
//! Prints `OK` or the rejection reason; the exit code follows. Used to score
//! model outputs against the exact gate the IDE enforces.

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: validate_scene <scene.py>");
        std::process::exit(2);
    };
    let script = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {path}: {e}");
            std::process::exit(2);
        }
    };
    match jade_build::manim::validate_scene_script(&script) {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("REJECTED: {e}");
            std::process::exit(1);
        }
    }
}
