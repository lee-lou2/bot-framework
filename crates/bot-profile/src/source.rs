//! bot-core의 `ProfileSource` 포트 구현. 반환 값은 집합 판본이고, 자세한 오류는 `load`가 돌려주는 `Report`로 본다.
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
    /// 파일 읽기는 동기라, 호스트가 필요하면 블로킹 풀에서 부른다.
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
