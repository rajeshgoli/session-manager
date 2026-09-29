use super::*;
#[derive(Args)]
pub(crate) struct MergeHoldArgs {
    number: Option<i64>,
    #[arg(long, conflicts_with = "number")]
    release: Option<i64>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long, requires = "number")]
    reason: Option<String>,
    #[arg(long)]
    json: bool,
}
pub(crate) fn run(client: &ApiClient, args: MergeHoldArgs) -> Result<()> {
    let Some(pr) = args.number.or(args.release) else {
        let value = client.get_json("/merge-holds")?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            for hold in value["holds"].as_array().into_iter().flatten() {
                println!(
                    "PR #{} ({}) held by {} since {}",
                    hold["pr"],
                    json_string(hold, "repo"),
                    json_string(hold, "placed_by"),
                    json_string(hold, "placed_at")
                );
            }
        }
        return Ok(());
    };
    let target = claims::resolve_claim_target(
        &git_repo::ProcessTools,
        &env::current_dir()?,
        args.repo.as_deref(),
    )?;
    let value=client.with_timeout(Duration::from_secs(90)).post_json(if args.release.is_some() {"/merge-holds/release"} else {"/merge-holds"},json!({"repo":target.repo,"pr":pr,"reason":args.reason,"requester_session_id":optional_current_session_id()}))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", result_line(&value, args.release.is_some()));
    }
    Ok(())
}
fn result_line(value: &Value, release: bool) -> String {
    let hold = &value["hold"];
    let repo = json_string(hold, "repo");
    let repo = repo.rsplit('/').next().unwrap_or(&repo);
    let prefix = format!("Merge hold on PR #{} ({repo})", hold["pr"]);
    if let Some(warning) = value["warning"].as_str() {
        return format!("{prefix} released. {warning}");
    }
    if release {
        return format!(
            "{prefix} released; {}.",
            if hold["made_draft"] == true {
                "the PR is ready for review again"
            } else {
                "the pre-existing draft is unchanged"
            }
        );
    }
    if value["already_held"] == true {
        format!(
            "{prefix} already held by {} since {}.",
            json_string(hold, "placed_by"),
            json_string(hold, "placed_at")
        )
    } else {
        format!("{prefix} placed; the PR is now a draft.")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_and_output() {
        assert!(Cli::try_parse_from(["sm", "merge-hold", "12", "--reason", "waiting"]).is_ok());
        assert!(Cli::try_parse_from(["sm", "merge-hold", "--release", "12"]).is_ok());
        assert!(Cli::try_parse_from(["sm", "merge-hold", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["sm", "merge-hold", "12", "--release", "12"]).is_err());
        let v = json!({"hold":{"pr":12,"repo":"owner/repo","made_draft":false}});
        assert_eq!(
            result_line(&v, false),
            "Merge hold on PR #12 (repo) placed; the PR is now a draft."
        );
        assert!(result_line(&v, true).contains("pre-existing draft is unchanged"));
    }
}
