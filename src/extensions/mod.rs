pub mod process;

use crate::model::{Declaration, Diagnostic};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub resource: String,
    pub observation: String,
    pub extension: String,
    /// An extension-owned location, used unchanged when requesting an edit.
    pub location: Value,
    pub writable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Occurrence {
    pub declaration: Declaration,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Extraction {
    pub occurrences: Vec<Occurrence>,
    pub diagnostics: Vec<Diagnostic>,
}
