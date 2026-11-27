//! Admin role wiring: Admin API/UI, operator secret delivery (terminal or 0600 file), and active-revision fencing.
//!
//! The random operator secret stays in memory and is delivered only to an interactive terminal or a
//! caller-specified 0600 file (`BOT_ADMIN_TOKEN_FILE`) — never piped stdout.

use super::{token_file::TokenFile, trace::Observe};
use crate::host::HostFile;
use bot_admin::{
    AdminDeps, StaticTokens,
    auth::{new_token, viewer},
};
use bot_core::{
    active::ActiveCheck,
    admin::{AdminAct, AdminRead},
    port::Broker,
    profile::ProfileCatalog,
};
use bot_gateway::{host::resolve_uid, identity};
use bot_profile::Registry;
use bot_store::Store;
use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub struct Deps {
    pub host: Arc<HostFile>,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    pub observe: Arc<Observe>,
    pub uid: u32,
    pub stop: CancellationToken,
    pub decisions: Arc<dyn Broker>,
    pub token_file: Option<PathBuf>,
    pub terminal: bool,
    pub active: Arc<dyn ActiveCheck>,
}

type Serving = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>;

#[derive(Debug, Eq, PartialEq)]
pub struct Delivery {
    pub terminal: bool,
    pub file: Option<PathBuf>,
}

pub fn delivery(terminal: bool, file: Option<&Path>) -> Result<Delivery, String> {
    if let Some(path) = file
        && !path.is_absolute()
    {
        return Err("BOT_ADMIN_TOKEN_FILE must be an absolute path".to_owned());
    }
    if !terminal && file.is_none() {
        return Err(
            "no channel to deliver Admin UI operator secret — run in a terminal or set BOT_ADMIN_TOKEN_FILE to an absolute path (0600)"
                .to_owned(),
        );
    }
    Ok(Delivery {
        terminal,
        file: file.map(Path::to_owned),
    })
}

pub struct Prepared {
    serving: Serving,
    addr: SocketAddr,
    token: String,
    terminal: bool,
    token_file: Option<TokenFile>,
}

fn operator_principal(host: &HostFile, uid: u32) -> Result<String, String> {
    host.gateway
        .cli
        .accounts
        .iter()
        .find(|(_, account)| resolve_uid(account).is_ok_and(|resolved| resolved == uid))
        .and_then(|(alias, _)| identity::local(alias))
        .map(|principal| principal.0)
        .ok_or_else(|| {
            format!(
                "no Admin UI operator identity — register this OS user (uid {uid}) under gateway.cli.accounts in host config"
            )
        })
}

pub async fn prepare(deps: &Deps) -> Result<Prepared, String> {
    let settings = deps.host.admin.clone();
    let principal = operator_principal(&deps.host, deps.uid)?;
    let delivery = delivery(deps.terminal, deps.token_file.as_deref())?;
    let token = new_token();
    // Claim lock before binding; existing token files stay untouched until `publish`.
    let token_file = match &delivery.file {
        Some(path) => Some(TokenFile::claim(path, deps.uid)?),
        None => None,
    };
    let auth = StaticTokens::new().with(&token, viewer(&principal, true));
    let listener = bot_admin::bind(settings.bind, settings.remote)
        .await
        .map_err(|e| format!("failed to bind Admin UI ({}): {e}", settings.bind))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("failed to read Admin UI address: {e}"))?;
    // Verify active revision before serving each response so stale processes return 503 with an empty body.
    let active = deps.active.clone();
    let config = settings
        .config(addr)
        .traces(deps.observe.exporter.clone())
        .logs(deps.observe.logs.clone())
        .issues(deps.observe.issues.clone())
        .leak_redactor(deps.observe.redactor.clone())
        .fence(move || {
            let active = active.clone();
            Box::pin(async move { active.verify().await.is_ok() })
        });
    let app = bot_admin::router(
        config,
        AdminDeps {
            read: deps.store.clone() as Arc<dyn AdminRead>,
            act: deps.store.clone() as Arc<dyn AdminAct>,
            broker: deps.decisions.clone(),
            auth: Arc::new(auth),
            profiles: deps.registry.clone() as Arc<dyn ProfileCatalog>,
        },
    )
    .ok_or("Admin UI has no accepted operator identity")?;
    let stop = deps.stop.clone();
    Ok(Prepared {
        serving: Box::pin(bot_admin::serve(listener, app, async move {
            stop.cancelled_owned().await
        })),
        addr,
        token,
        terminal: delivery.terminal,
        token_file,
    })
}

impl Prepared {
    /// Publishes the token file before committing the active revision so drop rolls back if activation fails.
    pub fn publish(&mut self) -> Result<(), String> {
        match self.token_file.as_mut() {
            Some(file) => file.publish(&self.token),
            None => Ok(()),
        }
    }

    pub fn open(self, tasks: &mut JoinSet<Result<(), String>>) {
        let Self {
            serving,
            addr,
            token,
            terminal,
            mut token_file,
        } = self;
        if let Some(file) = token_file.as_mut() {
            file.finalize();
        }
        println!("Admin UI: http://{addr}/ui");
        if terminal {
            println!("Username: anything, password: {token} (shown only in this terminal)");
        }
        if let Some(file) = &token_file {
            println!("Operator secret is in {} (0600)", file.path().display());
        }
        drop(token);
        tasks.spawn(async move {
            let served = serving.await.map_err(|e| format!("Admin UI stopped: {e}"));
            // Dropping `token_file` removes our published file and releases the lock on shutdown.
            drop(token_file);
            served
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operator_secret_goes_only_to_a_terminal_or_a_designated_file() {
        let file = Path::new("/run/bot/admin-token");
        assert_eq!(
            delivery(true, None).unwrap(),
            Delivery {
                terminal: true,
                file: None
            }
        );
        assert_eq!(
            delivery(false, Some(file)).unwrap(),
            Delivery {
                terminal: false,
                file: Some(file.to_owned())
            }
        );
        assert!(delivery(true, Some(file)).unwrap().terminal);
        let error = delivery(false, None).unwrap_err();
        assert!(error.contains("BOT_ADMIN_TOKEN_FILE"), "{error}");
        assert!(delivery(false, Some(Path::new("relative/token"))).is_err());
    }
}
