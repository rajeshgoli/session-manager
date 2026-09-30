//! Owner settings persistence (sm#1718, spec 1710 appendix D4). A child
//! module of `sessions`, so it shares the session store's write guard. The
//! settings sit next to `handoff_defaults` as a top-level object: one entry
//! per settings key, `{value, updated_at}`, holding only what the owner set.

use super::*;
use crate::owner_settings::{self, STORE_KEY};

impl SessionStore {
    /// The effective settings object (`GET /client/settings`).
    pub fn owner_settings(&self) -> Result<Value> {
        let state = self.load_parsed_state()?;
        Ok(owner_settings::effective(state.raw.get(STORE_KEY)))
    }

    /// Merge a `PUT /client/settings` body. `check` sees the effective
    /// settings before they are stored and may refuse them. The inner error
    /// is a validation message naming the field. Returns the effective
    /// settings object.
    pub fn update_owner_settings(
        &self,
        patch: &Value,
        check: impl FnOnce(&Value) -> Result<std::result::Result<(), String>>,
    ) -> Result<std::result::Result<Value, String>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let changed = match owner_settings::apply_patch(state.get(STORE_KEY), patch) {
            Ok(changed) => changed,
            Err(error) => return Ok(Err(error)),
        };
        set_owner_settings(&mut state, changed)?;
        let settings = owner_settings::effective(state.get(STORE_KEY));
        if let Err(error) = check(&settings)? {
            return Ok(Err(error));
        }
        self.write_raw_json_value(&state)?;
        Ok(Ok(settings))
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
