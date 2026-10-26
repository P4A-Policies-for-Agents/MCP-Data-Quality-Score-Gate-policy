use serde::Deserialize;
#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(alias = "aggregation")]
    pub aggregation: Option<String>,
    #[serde(alias = "blockBelow")]
    pub block_below: Option<f64>,
    #[serde(alias = "cdgcApiUrl", deserialize_with = "pdk::serde::deserialize_service")]
    pub cdgc_api_url: pdk::hl::Service,
    #[serde(
        alias = "cdgcLoginUrl",
        deserialize_with = "pdk::serde::deserialize_service"
    )]
    pub cdgc_login_url: pdk::hl::Service,
    #[serde(alias = "cdgcOrgPassword")]
    pub cdgc_org_password: String,
    #[serde(alias = "cdgcOrgUsername")]
    pub cdgc_org_username: String,
    #[serde(alias = "dimensions")]
    pub dimensions: Option<Vec<String>>,
    #[serde(alias = "distributed")]
    pub distributed: Option<bool>,
    #[serde(alias = "enforcement")]
    pub enforcement: Option<String>,
    #[serde(alias = "failOpenOnIdmcError")]
    pub fail_open_on_idmc_error: Option<bool>,
    #[serde(alias = "honourRuleThresholds")]
    pub honour_rule_thresholds: Option<bool>,
    #[serde(alias = "maxScoreAgeHours")]
    pub max_score_age_hours: Option<i64>,
    #[serde(alias = "mcpServerAssetId")]
    pub mcp_server_asset_id: String,
    #[serde(alias = "scoreRefreshSeconds")]
    pub score_refresh_seconds: Option<i64>,
    #[serde(alias = "staleAction")]
    pub stale_action: Option<String>,
    #[serde(alias = "timeout")]
    pub timeout: Option<i64>,
    #[serde(alias = "toolClassTypes")]
    pub tool_class_types: Option<Vec<String>>,
    #[serde(alias = "unscoredAction")]
    pub unscored_action: Option<String>,
    #[serde(alias = "warnBelow")]
    pub warn_below: Option<f64>,
}
#[pdk::hl::entrypoint_flex]
fn init(abi: &dyn pdk::flex_abi::api::FlexAbi) -> Result<(), anyhow::Error> {
    let config: Config = serde_json::from_slice(abi.get_configuration())
        .map_err(|err| {
            anyhow::anyhow!(
                "Failed to parse configuration '{}'. Cause: {}",
                String::from_utf8_lossy(abi.get_configuration()), err
            )
        })?;
    abi.service_create(config.cdgc_api_url)?;
    abi.service_create(config.cdgc_login_url)?;
    abi.setup()?;
    Ok(())
}
