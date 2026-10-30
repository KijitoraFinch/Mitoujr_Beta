use crate::model::{Region, RegionSelector};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Endpoint {
    pub selector: RegionSelector,
    pub region: Region,
}
