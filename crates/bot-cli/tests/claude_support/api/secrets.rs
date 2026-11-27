//! Mutable in-memory `SecretStore` double for testing live key rotation and revocation.
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

    pub fn set(&self, name: &str, value: &str) {
        self.values
            .lock()
            .unwrap()
            .insert(name.to_owned(), value.as_bytes().to_vec());
    }

    pub fn remove(&self, name: &str) {
        self.values.lock().unwrap().remove(name);
    }

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
