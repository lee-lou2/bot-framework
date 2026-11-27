//! Shared host, profile set, entrance route, and secret store loading.
use crate::{
    host::{self, HostFile},
    secrets::admin::SecretAdmin,
    settings::Settings,
};
use bot_core::port::SecretStore;
use bot_gateway::{
    cli::socket::current_uid,
    host::resolve_uid,
    routes::{Active, Routes},
};
use bot_profile::{Limits, ProfileSet, Registry};
use std::sync::Arc;

pub struct Loaded {
    pub host: Arc<HostFile>,
    pub registry: Arc<Registry>,
    pub set: Arc<ProfileSet>,
    pub uid: u32,
}

pub fn load_host(settings: &Settings, uid: u32) -> Result<HostFile, String> {
    let mut host = match &settings.host_config {
        Some(path) => host::load(path).map_err(|e| e.to_string())?,
        None => {
            let mut demo = host::demo(uid);
            demo.attach_demo_server(settings.profiles.as_deref());
            demo
        }
    };
    // Resolve OS user names once so fingerprint, routes, and admin identity share one UID table.
    host.pin_uids()?;
    Ok(host)
}

pub fn load(settings: &Settings) -> Result<Loaded, String> {
    let root = settings
        .profiles
        .as_ref()
        .ok_or("BOT_PROFILES is required (profiles root directory)")?;
    let uid = current_uid().map_err(|e| e.to_string())?;
    let host = load_host(settings, uid)?;
    let registry = Arc::new(Registry::new());
    let set = registry
        .reload(root, &host.catalog, &Limits::default())
        .map_err(|report| report.to_string())?;
    Ok(Loaded {
        host: Arc::new(host),
        registry,
        set,
        uid,
    })
}

pub fn check_entrances(active: Active, host: &HostFile) -> Result<Routes, String> {
    Routes::build(active, &host.gateway, &resolve_uid).map_err(|e| e.to_string())
}

pub fn secret_admin(settings: &Settings) -> Result<Arc<SecretAdmin>, String> {
    SecretAdmin::from_settings(settings).map(Arc::new)
}

pub fn secret_store(settings: &Settings) -> Result<Arc<dyn SecretStore>, String> {
    Ok(secret_admin(settings)?)
}
