//! Live check of the chat layer, before any UI exists.
//!
//!     export ANTHROPIC_API_KEY=...
//!     cargo run -p jade-ai --example chat_smoke
//!     cargo run -p jade-ai --example chat_smoke -- --json
//!
//! `--json` exercises the `output_config.format` path the Visualize feature
//! uses; without it, the plain streaming path the Explain feature uses.

use std::time::{Duration, Instant};

use jade_ai::chat::{ChatBackend, ChatDelta, ChatRequest, Effort, Lane};

#[tokio::main]
async fn main() {
    let json_mode = std::env::args().any(|a| a == "--json");

    let backend = ChatBackend::new();
    let (key, source) = backend.credential();
    println!("model:      {}", backend.model().label());
    println!("credential: {} ({})", if key.is_some() { "found" } else { "MISSING" }, source.label());
    if key.is_none() {
        eprintln!("\nSet ANTHROPIC_API_KEY and try again.");
        std::process::exit(1);
    }

    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title", "steps"],
        "properties": {
            "title": { "type": "string" },
            "steps": { "type": "array", "items": { "type": "string" } }
        }
    });

    let req = ChatRequest {
        system: "You explain code briefly and plainly.".into(),
        user: if json_mode {
            "Describe what a bubble sort does, as a title and three steps.".into()
        } else {
            "In two sentences, what does `for (auto& v : xs) v *= 2;` do?".into()
        },
        max_tokens: 1024,
        effort: Effort::Low,
        json_schema: json_mode.then_some(schema),
        timeout: Duration::from_secs(60),
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let started = Instant::now();
    let gen = backend.start(Lane::Explain, req, tx);
    println!("generation: {gen}\n---");

    let mut first_token: Option<Duration> = None;
    let mut body = String::new();
    while let Some(delta) = rx.recv().await {
        match delta {
            ChatDelta::Started(p) => println!("[started {:?} via {p:?}]", started.elapsed()),
            ChatDelta::Thinking => println!("[thinking]"),
            ChatDelta::Text(t) => {
                first_token.get_or_insert_with(|| started.elapsed());
                print!("{t}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                body.push_str(&t);
            }
            ChatDelta::Done { stop_reason } => {
                println!("\n---\nstop: {stop_reason:?}");
                break;
            }
            ChatDelta::Failed(e) => {
                println!("\n---\nFAILED: {} — {:?}", e.headline(), e.detail());
                std::process::exit(1);
            }
        }
    }
    backend.finished(Lane::Explain, gen);

    println!("first token: {:?}", first_token);
    println!("total:       {:?}", started.elapsed());

    if json_mode {
        match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => println!("valid JSON, keys: {:?}", v.as_object().map(|o| o.keys().collect::<Vec<_>>())),
            Err(e) => {
                println!("NOT valid JSON: {e}");
                std::process::exit(1);
            }
        }
    }
}
