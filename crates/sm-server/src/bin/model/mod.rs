use super::*;
#[derive(Args)]
pub(super) struct ModelArgs {
    #[command(subcommand)]
    command: ModelCommand,
}
#[derive(Subcommand)]
enum ModelCommand {
    Status,
    Load {
        key: String,
        #[arg(long)]
        seats: Option<u32>,
        #[arg(long)]
        context: Option<u64>,
        /// Reservation in decimal GB, required for unmeasured MTPLX models.
        #[arg(long)]
        reservation: Option<f64>,
    },
    Unload {
        #[arg(long)]
        force: bool,
    },
}
pub(super) fn run(client: &ApiClient, args: ModelArgs) -> Result<()> {
    // Covers drain + stop, or a cold load, without the usual 5s CLI deadline.
    let client = client.with_timeout(Duration::from_secs(1500));
    let payload = match args.command {
        ModelCommand::Status => client.get_json("/client/model")?,
        ModelCommand::Load {
            key,
            seats,
            context,
            reservation,
        } => client.post_json(
            "/client/model/load",
            json!({"key":key,"seats":seats,"context":context,"reservation":reservation}),
        )?,
        ModelCommand::Unload { force } => {
            client.post_json("/client/model/unload", json!({"force":force}))?
        }
    };
    if payload["model"].is_null() {
        println!("no local model loaded");
        return Ok(());
    }
    let model = &payload["model"];
    println!(
        "{}  {}  server {}  seats {}/{} used (+{} judge)  reservation {:.1} GB",
        model["identifier"].as_str().unwrap_or("unknown"),
        model["state"].as_str().unwrap_or("unknown"),
        model["server"].as_str().unwrap_or("unknown"),
        payload["seats_used"],
        model["seats"],
        payload["judge_seats"],
        model["reservation_bytes"].as_f64().unwrap_or(0.) / 1_000_000_000.
    );
    if let Some(reason) = model["last_yield_reason"].as_str() {
        println!(
            "last yield: {} ({reason})",
            model["last_yield_at"].as_str().unwrap_or("unknown")
        );
    }
    if let Some(error) = model["last_error"].as_str() {
        println!("error: {error}");
    }
    Ok(())
}
