use super::*;
#[derive(Args)]
pub(crate) struct ArtifactArgs {
    #[arg(long, global = true)]
    root: Option<String>,
    #[command(subcommand)]
    command: ArtifactCommand,
}
#[derive(Subcommand)]
enum ArtifactCommand {
    New {
        name: String,
        #[command(flatten)]
        flags: Flags,
    },
    Record {
        #[arg(required=true,num_args=1..)]
        names: Vec<String>,
        #[command(flatten)]
        flags: Flags,
    },
    Freeze {
        name: String,
    },
    Thaw {
        name: String,
        #[arg(long)]
        wait: bool,
    },
    Rebuild {
        name: String,
    },
    Done {
        name: String,
    },
    Consumer {
        name: String,
        #[arg(value_parser=["add","remove"])]
        operation: String,
        consumer: String,
    },
    Mark {
        name: String,
        #[arg(long, conflicts_with = "no_project")]
        project: bool,
        #[arg(long)]
        no_project: bool,
        #[arg(long, conflicts_with = "no_keep_until")]
        keep_until: Option<String>,
        #[arg(long)]
        no_keep_until: bool,
    },
    Delete {
        name: String,
        #[arg(long)]
        reason: String,
    },
    List {
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        epic: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
    },
    Disk,
    /// Measure a folder without changing its retention policy. Run bulk walks via sm queue.
    Measure {
        name: String,
    },
    Unclaimable {
        name: String,
        #[arg(long)]
        note: String,
    },
    Report {
        name: String,
        #[arg(long)]
        note: String,
        #[arg(long,value_parser=["record","keep","delete"])]
        recommend: String,
    },
}
#[derive(Args, serde::Serialize)]
struct Flags {
    #[arg(long)]
    note: String,
    #[arg(long)]
    describe: Option<String>,
    #[arg(long = "consumer")]
    consumers: Vec<String>,
    #[arg(long)]
    project: bool,
    #[arg(long)]
    keep_until: Option<String>,
    #[arg(long)]
    recipe_complete: bool,
    #[arg(long)]
    gpu_hours: Option<f64>,
    #[arg(long)]
    gpu_cost: Option<f64>,
    #[arg(long)]
    ticket: Option<i64>,
    #[arg(long)]
    epic: Option<String>,
}
impl Flags {
    fn body(mut self) -> Result<Value> {
        if let Some(path) = self.describe.as_deref().and_then(|d| d.strip_prefix('@')) {
            self.describe = Some(fs::read_to_string(path)?);
        }
        Ok(serde_json::to_value(self)?)
    }
}
pub(crate) fn run(client: &ApiClient, args: ArtifactArgs) -> Result<()> {
    let repo = env::current_dir()
        .ok()
        .and_then(|p| git_repo::resolve_repo_slug(&git_repo::ProcessTools, &p).ok());
    let mut query = String::new();
    for (key, value) in [("root", args.root.as_ref()), ("repo", repo.as_ref())] {
        if let Some(value) = value {
            query.push(if query.is_empty() { '?' } else { '&' });
            query.push_str(&format!("{key}={}", encode(value)));
        }
    }
    let mut create = false;
    let mut wait_for_thaw = false;
    let (name, action, mut body) = match args.command {
        ArtifactCommand::List { state, epic, json } => {
            for (key, value) in [("state", state), ("epic", epic)] {
                if let Some(value) = value {
                    query.push(if query.is_empty() { '?' } else { '&' });
                    query.push_str(&format!("{key}={}", encode(&value)));
                }
            }
            let v = client.get_json(&format!("/artifacts{query}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for a in v["artifacts"].as_array().into_iter().flatten() {
                    println!("{}  {}  {}  {}  disk={} data={} frozen={}  consumers={}  {}  backup_due={}",json_string(a,"name"),json_string(a,"state"),json_string(a,"location"),if a["hot"]==true{"hot"}else{"cold"},a["disk_bytes"],a["data_bytes"],a["frozen_bytes"],a["consumers"],if a["gpu_hours"].as_f64().unwrap_or(0.0)>0.0 || a["gpu_cost_usd"].as_f64().unwrap_or(0.0)>0.0{"GPU".into()}else if a["rebuildable"]==true{format!("{:.0} min",a["rebuild_seconds"].as_f64().unwrap_or(0.0)/60.0)}else{"not rebuildable".into()},a["backup_due"]);
                }
            }
            return Ok(());
        }
        ArtifactCommand::Show { name } => {
            let mut v = client.get_json(&format!("/artifacts/{}{query}", encode(&name)))?;
            if let Some(events) = v["events"].as_array_mut() {
                events.truncate(20);
            }
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
        ArtifactCommand::Disk => {
            let v = client.get_json(&format!("/artifacts{query}"))?;
            println!("{}", serde_json::to_string_pretty(&v["filesystems"])?);
            return Ok(());
        }
        ArtifactCommand::New { name, flags } => {
            create = true;
            (name, "new", flags.body()?)
        }
        ArtifactCommand::Record { names, flags } => {
            let mut b = flags.body()?;
            b["names"] = json!(names);
            (names[0].clone(), "record", b)
        }
        ArtifactCommand::Freeze { name } => (name, "freeze", json!({})),
        ArtifactCommand::Thaw { name, wait } => {
            wait_for_thaw = wait;
            (name, "thaw", json!({}))
        }
        ArtifactCommand::Rebuild { name } => (name, "rebuild", json!({})),
        ArtifactCommand::Measure { name } => (name, "measure", json!({})),
        ArtifactCommand::Done { name } => (name, "done", json!({})),
        ArtifactCommand::Consumer {
            name,
            operation,
            consumer,
        } => (
            name,
            "consumers",
            json!({"operation":operation,"consumer":consumer}),
        ),
        ArtifactCommand::Mark {
            name,
            project,
            no_project,
            keep_until,
            no_keep_until,
        } => {
            let mut b = json!({});
            if project || no_project {
                b["project"] = json!(project);
            }
            if let Some(date) = keep_until {
                b["keep_until"] = json!(date);
            } else if no_keep_until {
                b["keep_until"] = Value::Null;
            }
            (name, "marks", b)
        }
        ArtifactCommand::Delete { name, reason } => (name, "delete", json!({"reason":reason})),
        ArtifactCommand::Unclaimable { name, note } => (name, "unclaimable", json!({"note":note})),
        ArtifactCommand::Report {
            name,
            note,
            recommend,
        } => (name, "report", json!({"note":note,"recommend":recommend})),
    };
    body["root"] = json!(args.root);
    body["repo"] = json!(repo);
    body["name"] = json!(name);
    // Queue jobs start without the agent's environment, so this is where a job
    // that runs `sm artifact thaw` lands.
    let id = optional_current_session_id().ok_or_else(|| {
        anyhow!(
            "sm artifact {action} needs SESSION_MANAGER_ID, which queue jobs do not have; \
             to thaw {name} for a queue job, run `sm queue run --input {name} ...` \
             or name its path in the command, and sm thaws it before the job starts"
        )
    })?;
    let path = if create {
        "/artifacts".into()
    } else {
        format!("/artifacts/{}/{action}", encode(&name))
    };
    let mut v = client
        .request_with_headers("POST", &path, Some(body), &[("X-SM-Session-ID", &id)])?
        .into_json()?;
    if wait_for_thaw {
        let operation_id = v["operation"]["id"].clone();
        loop {
            let status = client.get_json(&format!("/artifacts/{}{query}", encode(&name)))?;
            if status["operation"]["id"] != operation_id {
                bail!("artifact operation changed while waiting");
            }
            if status["operation"]["phase"] == "failed" {
                bail!("thaw failed: {}", status["operation"]["error"]);
            }
            if status["state"] == "live" && status["op"].is_null() {
                v = status;
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    if create {
        println!("{}/{}", json_string(&v, "root"), json_string(&v, "name"));
    } else {
        println!("{}", serde_json::to_string_pretty(&v)?);
    }
    Ok(())
}
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
