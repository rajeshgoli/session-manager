use super::*;
#[derive(Args, Default)]
pub(super) struct QueueArtifactArgs {
    #[arg(long = "artifact")]
    outputs: Vec<String>,
    #[arg(long = "input")]
    inputs: Vec<String>,
    #[arg(long)]
    disk: Option<String>,
    #[arg(long)]
    artifact_root: Option<String>,
    #[arg(long)]
    note: Option<String>,
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
    ticket: Option<i64>,
    #[arg(long)]
    epic: Option<String>,
}
impl QueueArtifactArgs {
    pub(super) fn apply(mut self, body: &mut Value) -> Result<()> {
        let has_record = self.note.is_some()
            || self.describe.is_some()
            || !self.consumers.is_empty()
            || self.project
            || self.keep_until.is_some()
            || self.recipe_complete
            || self.ticket.is_some()
            || self.epic.is_some();
        if has_record && self.outputs.is_empty() {
            bail!("record flags require --artifact");
        }
        if !self.outputs.is_empty() && self.disk.is_none() {
            bail!("--artifact requires --disk");
        }
        if self.outputs.is_empty() && self.inputs.is_empty() && self.disk.is_none() && !has_record {
            return Ok(());
        }
        if let Some(path) = self.describe.as_deref().and_then(|v| v.strip_prefix('@')) {
            self.describe = Some(fs::read_to_string(path)?);
        }
        let record = if has_record {
            Some(sm_server::managed_artifacts::RecordFlags {
                note: self.note.context("record flags require --note")?,
                describe: self.describe,
                consumers: self.consumers,
                project: self.project,
                keep_until: self.keep_until,
                recipe_complete: self.recipe_complete,
                ticket: self.ticket,
                epic: self.epic,
                ..Default::default()
            })
        } else {
            None
        };
        let declarations = sm_server::managed_artifacts::submission::Declarations {
            outputs: self.outputs,
            inputs: self.inputs,
            disk_bytes: self
                .disk
                .as_deref()
                .map(parse_queue_memory_bytes)
                .transpose()?,
            record,
        };
        body["artifacts"] = serde_json::to_value(declarations)?;
        body["artifact_root"] = json!(self.artifact_root);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flags_build_the_queue_artifact_payload() {
        let cli = Cli::try_parse_from([
            "sm",
            "queue",
            "run",
            "--artifact",
            "out",
            "--disk",
            "2G",
            "--note",
            "Dataset",
            "--consumer",
            "#42",
            "--recipe-complete",
            "--",
            "true",
        ])
        .unwrap();
        let Command::Queue(args) = cli.command else {
            panic!("queue")
        };
        let QueueCommand::Run(args) = args.command else {
            panic!("run")
        };
        let mut body = json!({});
        args.artifacts.apply(&mut body).unwrap();
        assert_eq!(body["artifacts"]["outputs"], json!(["out"]));
        assert_eq!(body["artifacts"]["record"]["consumers"], json!(["#42"]));
        assert!(body["artifacts"]["disk_bytes"].as_i64().unwrap() > 0);
    }
    #[test]
    fn record_flags_require_an_output_and_output_requires_disk() {
        let mut body = json!({});
        assert!(QueueArtifactArgs {
            note: Some("note".into()),
            ..Default::default()
        }
        .apply(&mut body)
        .is_err());
        assert!(QueueArtifactArgs {
            outputs: vec!["out".into()],
            ..Default::default()
        }
        .apply(&mut body)
        .is_err());
        QueueArtifactArgs::default().apply(&mut body).unwrap();
        assert_eq!(body, json!({}));
    }
}
