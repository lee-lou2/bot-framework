//! 시험이 키 값을 바꾸고 지울 수 있는 합성 비밀 보관소(실행 중인 키 회전·철회를 흉내 낸다): 어떤 프로필로 열어도 같은 이름·값을 주고, 없는 이름은 «값 없음»(`Missing`)이다.
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretStore, SecretValue},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct ApiSecrets {
    values: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl ApiSecrets {
    pub fn new(entries: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            values: Arc::new(Mutex::new(
                entries
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), value.as_bytes().to_vec()))
                    .collect(),
            )),
        })
    }

    /// 이 이름의 값을 바꾼다(키 회전)
    pub fn set(&self, name: &str, value: &str) {
        self.values
            .lock()
            .unwrap()
            .insert(name.to_owned(), value.as_bytes().to_vec());
    }

    /// 이 이름의 값을 지운다(키 철회: 이제 «값 없음»이다)
    pub fn remove(&self, name: &str) {
        self.values.lock().unwrap().remove(name);
    }

    /// 지금 보관된 값 전부(저장소에 평문으로 새지 않았는지 쓸어 볼 때 쓴다)
    pub fn values(&self) -> Vec<String> {
        self.values
            .lock()
            .unwrap()
            .values()
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .collect()
    }
}

struct Bound(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl SecretStore for ApiSecrets {
    fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let values = self.values.clone();
        Box::pin(async move { Ok(Box::new(Bound(values)) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for Bound {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let found = self.0.lock().unwrap().get(name.as_str()).cloned();
        Box::pin(async move { found.map(SecretValue).ok_or(Error::Missing) })
    }
}
