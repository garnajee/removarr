//! Jellyfin model.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::torrent::MappedPath;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinItemRef {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub path: MappedPath,
    pub series: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub provider_ids: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinLibrary {
    pub name: String,
    pub locations: Vec<MappedPath>,
}
