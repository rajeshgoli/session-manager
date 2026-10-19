use super::*;

#[derive(Args)]
pub(super) struct ShellArgs {
    #[command(subcommand)]
    action: Option<ShellCommand>,
    /// Working directory (defaults to the current directory)
    dir: Option<String>,
}
#[derive(Subcommand)]
enum ShellCommand {
    Ls,
    Rm { id: String },
}

pub(super) fn run(client: &ApiClient, args: ShellArgs) -> Result<()> {
    match args.action {
        Some(ShellCommand::Ls) => {
            let payload = client.get_json("/terminals")?;
            for t in payload["terminals"]
                .as_array()
                .context("Missing terminal list")?
            {
                println!(
                    "{} {} {}",
                    t["id"].as_str().unwrap_or(""),
                    if t["busy_since"].is_null() {
                        "idle"
                    } else {
                        "running"
                    },
                    t["cwd"].as_str().unwrap_or("")
                );
            }
        }
        Some(ShellCommand::Rm { id }) => {
            client.delete_json(
                &format!("/terminals/{}", encode_path_segment(&id)),
                json!({}),
            )?;
            println!("Retired {id}");
        }
        None => {
            let cwd = args.dir.map(PathBuf::from).unwrap_or(env::current_dir()?);
            let t = client.post_json("/terminals", json!({"cwd":cwd}))?;
            let id = t["id"].as_str().context("Missing terminal id")?;
            println!("{id}");
            io::stdout().flush()?;
            if cfg!(target_os = "macos")
                && matches!(client.host.as_str(), "localhost" | "127.0.0.1" | "::1")
                && io::stdin().is_terminal()
                && io::stdout().is_terminal()
            {
                let mut command = process::Command::new("tmux");
                if let Some(socket) = t["tmux_socket_name"].as_str() {
                    command.args(["-L", socket]);
                }
                command
                    .env_remove("TMUX")
                    .args(["attach-session", "-t", &format!("={id}")]);
                if !command.status()?.success() {
                    bail!("Attach failed; shell {id} is still open. Run sm shell rm {id} to retire it.");
                }
            } else {
                let route =
                    client.post_json(&format!("/terminals/{id}/route"), json!({"where":"home"}))?;
                println!(
                    "{}",
                    route["command"].as_str().context("Missing SSH command")?
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_shell_forms() {
        for argv in [
            vec!["sm", "shell"],
            vec!["sm", "shell", "/tmp"],
            vec!["sm", "shell", "ls"],
            vec!["sm", "shell", "rm", "term-abcd"],
        ] {
            assert!(Cli::try_parse_from(argv).is_ok());
        }
    }
}
