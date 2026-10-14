use nektome_client::{app::App, ui};

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().collect();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!(
            "NektoMe Rust client\n\nUsage: nektome-client [--cli]\n\nOptions:\n  --cli       run the terminal chat client\n  -h, --help  show this help\n  --version   show the version"
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--version") {
        println!("nektome-client {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--cli") {
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(App::run())
    } else {
        ui::run()
    }
}
