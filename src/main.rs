#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("moocp=info")
        .init();
    if let Err(error) = moocp::cli::run().await {
        eprintln!("moocp: {error}");
        std::process::exit(1);
    }
}
