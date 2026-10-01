//! `sm bug show`: the private part of a bug filed from the sm app (sm#1859,
//! ticket #1875). The public issue's footer names the id; this prints the
//! text, page data and server facts from `GET /bugs/{id}` and saves the
//! screenshot where the agent can open it.

use super::*;
use base64::{engine::general_purpose::STANDARD, Engine as _};

#[derive(Args)]
pub(crate) struct BugArgs {
    #[command(subcommand)]
    command: BugCommand,
}

#[derive(Subcommand)]
enum BugCommand {
    /// Print a bug report's text, page data and server facts; save its screenshot
    Show(BugShowArgs),
}

#[derive(Args)]
struct BugShowArgs {
    /// The id from the issue footer, with or without `BR-`
    id: String,
    /// Print the server's JSON verbatim
    #[arg(long)]
    json: bool,
}

pub(crate) fn run_bug(client: &ApiClient, args: BugArgs) -> Result<()> {
    match args.command {
        BugCommand::Show(args) => show(client, args),
    }
}

fn show(client: &ApiClient, args: BugShowArgs) -> Result<()> {
    let id = normalize_id(&args.id);
    let path = format!("/bugs/{}", encode_path_segment(&id));
    let response = client.request("GET", &path, None)?;
    if response.status == 404 {
        eprintln!("No bug report {id}: sm keeps only the latest reports.");
        process::exit(1);
    }
    if args.json {
        if !(200..300).contains(&response.status) {
            return Err(response.into_json().unwrap_err());
        }
        println!("{}", response.body.trim_end());
        return Ok(());
    }
    let report = response.into_json()?;
    for line in show_lines(&report) {
        println!("{line}");
    }
    if report["has_screenshot"].as_bool() == Some(true) {
        let response = client.request(
            "GET",
            &format!("{path}/screenshot.png?encoding=base64"),
            None,
        )?;
        if !(200..300).contains(&response.status) {
            return Err(response.into_status_error());
        }
        let file = write_screenshot(&env::temp_dir(), &id, &response.body)?;
        println!("Screenshot: {}", file.display());
    }
    Ok(())
}

/// `BR-…` whether or not the caller typed the prefix.
fn normalize_id(id: &str) -> String {
    let id = id.trim();
    if id.starts_with("BR-") {
        id.to_owned()
    } else {
        format!("BR-{id}")
    }
}

/// The text, a metadata block, then page data and server facts.
fn show_lines(report: &Value) -> Vec<String> {
    let text_of = |value: &Value| value.as_str().unwrap_or("—").to_owned();
    let mut lines = vec![
        report["text"].as_str().unwrap_or_default().to_owned(),
        String::new(),
    ];
    let issue = match report["issue"].as_object() {
        Some(issue) => format!(
            "{}#{} {}",
            issue["repo"].as_str().unwrap_or_default(),
            issue["number"],
            issue["url"].as_str().unwrap_or_default()
        ),
        None => "not filed".to_owned(),
    };
    for (label, value) in [
        ("Bug", text_of(&report["bug_id"])),
        ("Filed", text_of(&report["created_at"])),
        ("Issue", issue),
        ("Client", text_of(&report["client"])),
        ("Version", text_of(&report["client_version"])),
        ("Page", text_of(&report["page"])),
        ("Route", text_of(&report["route"])),
    ] {
        lines.push(format!("{label:<8} {value}"));
    }
    for (label, key) in [("Page data", "page_data"), ("Server facts", "server_facts")] {
        lines.push(String::new());
        lines.push(format!("{label}:"));
        lines.push(serde_json::to_string_pretty(&report[key]).unwrap_or_default());
    }
    lines
}

/// Saves the base64 screenshot as `<dir>/sm-bug-<id>.png`.
fn write_screenshot(dir: &Path, id: &str, base64: &str) -> Result<PathBuf> {
    let png = STANDARD
        .decode(base64.trim())
        .context("the server sent a screenshot that is not base64")?;
    let path = dir.join(format!("sm-bug-{id}.png"));
    fs::write(&path, png).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests;
