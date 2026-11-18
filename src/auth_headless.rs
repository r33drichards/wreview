//! WASIX hosting never opens browsers or reads a native credential store.
use anyhow::{Result, bail};
pub fn login() -> Result<()> {
    bail!(
        "Browser login is available in the native CLI only; provision CHATGPT_ACCESS_TOKEN through your host's secret store"
    )
}
pub fn logout() -> Result<()> {
    bail!(
        "Remove CHATGPT_ACCESS_TOKEN from your host's secret store and revoke the app in ChatGPT settings"
    )
}
pub fn access_token() -> Result<String> {
    bail!("Set CHATGPT_ACCESS_TOKEN through your host's secret store")
}
