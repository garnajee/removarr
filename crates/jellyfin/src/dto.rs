//! Wire types. Deliberately lenient about optional fields, strict about shape: a wrong type
//! or a missing mandatory field is a malformed response, never a guess.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct SystemInfo {
    pub version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct VirtualFolder {
    pub name: String,
    #[serde(default)]
    pub locations: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ItemsPage {
    pub items: Vec<RawItem>,
    pub total_record_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct RawItem {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub path: Option<String>,
    pub series_name: Option<String>,
    pub series_id: Option<String>,
    pub parent_index_number: Option<i32>,
    pub index_number: Option<i32>,
    pub index_number_end: Option<i32>,
    #[serde(default)]
    pub provider_ids: Option<BTreeMap<String, Option<String>>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ScheduledTask {
    pub key: Option<String>,
    pub state: Option<String>,
}
