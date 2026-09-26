pub mod process;

use crate::model::{Diagnostic, Link};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A Link interpreted from the supplied, immutable Observation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportCandidate {
    pub link: Link,
    pub location: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportResult {
    pub links: Vec<ImportCandidate>,
    pub diagnostics: Vec<Diagnostic>,
}
