//! Owner settings persistence (sm#1718, spec 1710 appendix D4). A child
//! module of `sessions`, so it shares the session store's write guard. The
//! settings sit next to `handoff_defaults` as a top-level object: one entry
//! per settings key, `{value, updated_at}`, holding only what the owner set.

use super::*;
use crate::config::BoardStartDefaults;
use crate::owner_settings::{self, STORE_KEY};

impl SessionStore {
    /// The effective settings object (`GET /client/settings`). With no
    /// stored `new_agent`, the first read seeds it from `seed`
    /// (`board.start_defaults`) when config has one.
    pub fn owner_settings(&self, seed: Option<&BoardStartDefaults>) -> Result<Value> {
        let state = self.load_parsed_state()?;
        let stored = state.raw.get(STORE_KEY);
        if let Some(seed) =
            seed.filter(|_| owner_settings::stored_value(stored, "new_agent").is_none())
        {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            if owner_settings::stored_value(state.get(STORE_KEY), "new_agent").is_none() {
                // A seed that fails every check stores nothing set, so it
                // is not retried on every read.
                let seed = owner_settings::seed_new_agent(seed).unwrap_or_else(|| json!({}));
                set_owner_settings(&mut state, [("new_agent".to_owned(), seed)])?;
                self.write_raw_json_value(&state)?;
            }
            return Ok(owner_settings::effective(state.get(STORE_KEY)));
        }
        Ok(owner_settings::effective(stored))
    }

    /// Merge a `PUT /client/settings` body. The inner error is a validation
    /// message naming the field. Returns the effective settings object.
    pub fn update_owner_settings(
        &self,
        patch: &Value,
    ) -> Result<std::result::Result<Value, String>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let changed = match owner_settings::apply_patch(state.get(STORE_KEY), patch) {
            Ok(changed) => changed,
            Err(error) => return Ok(Err(error)),
        };
        set_owner_settings(&mut state, changed)?;
        self.write_raw_json_value(&state)?;
        Ok(Ok(owner_settings::effective(state.get(STORE_KEY))))
    }
}

fn set_owner_settings(
    state: &mut Value,
    changed: impl IntoIterator<Item = (String, Value)>,
) -> Result<()> {
    let state = state
        .as_object_mut()
        .context("session state is not a JSON object")?;
    let settings = state.entry(STORE_KEY).or_insert_with(|| json!({}));
    if !settings.is_object() {
        *settings = json!({});
    }
    let updated_at = now_rfc3339();
    for (key, value) in changed {
        settings[key.as_str()] = json!({ "value": value, "updated_at": updated_at });
    }
    Ok(())
}
