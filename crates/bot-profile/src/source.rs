//! Implements `bot_core::port::ProfileSource` over the local filesystem.
use crate::{host::HostCatalog, limits::Limits, registry::load};
use bot_core::{
    id::Revision,
    port::{Error, ProfileSource, Reply},
};
use std::path::Path;

pub struct FsProfiles {
    pub host: HostCatalog,
    pub limits: Limits,
}

impl ProfileSource for FsProfiles {
    /// Performs synchronous disk I/O; callers should offload to a blocking pool if needed.
    fn validate<'a>(&'a self, root: &'a Path) -> Reply<'a, Revision> {
        Box::pin(async move {
            match load(root, &self.host, &self.limits) {
                Ok(set) => Ok(set.revision().clone()),
                Err(report) if report.has("root-missing") || report.has("root-not-dir") => {
                    Err(Error::Missing)
                }
                Err(_) => Err(Error::Invalid),
            }
        })
    }
}
