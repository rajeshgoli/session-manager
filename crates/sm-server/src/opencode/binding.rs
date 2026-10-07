//! Host runtime metadata persisted before an opencode process is launched.
use std::path::{Component, Path};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeBinding {
    pub port: u16,
    pub state_dir: String,
    pub version: String,
    pub model_base_url: String,
}

impl RuntimeBinding {
    /// Launch still checks the physical path, port reservation, installed
    /// version and model availability. A deserialized record is not authority
    /// to read arbitrary state or launch without those checks.
    pub fn validate(&self) -> Result<()> {
        let state = Path::new(&self.state_dir);
        if self.port == 0 || self.version.trim().is_empty() {
            bail!("opencode runtime binding needs a port and version");
        }
        if !state.is_absolute()
            || state == Path::new("/")
            || self.state_dir.contains('\0')
            || state
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            bail!("opencode runtime state directory must be an absolute path without traversal");
        }
        super::loopback_url(&self.model_base_url)?;
        Ok(())
    }
}
