//! 두 발송자가 하나의 실제 PG outbox를 독립적으로 처리한다.
mod process_support;

use bot_core::{
    command::{Accept, Envelope, Finish, JsonPayload, Target},
    id::*,
    intake::{DeliveryScope, Outgoing, PushTransportId},
    port::RunStore,
    profile::{ActiveRevisions, Adapter, Snapshot},
    state::{Conversation, ConversationKind},
};
use bot_gateway::{
    delivery::{Deliverer, Delivery, Sender},
    host::HostConfig,
    ingress::Ingress,
    reply::ReplyBody,
    testing::{MemoryPrivate, TestCatalog, profile},
};
use bot_store::{Config, Store};
use process_support::Schema;
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

struct BoundSender {
    scope: DeliveryScope,
    binding: &'static str,
    seen: Mutex<Vec<String>>,
    sent: Mutex<Vec<String>>,
}

impl BoundSender {
    fn new(binding: &'static str, key: PushTransportId, partitioned: bool) -> Arc<Self> {
        let scope = DeliveryScope {
            transport: key,
            binding: partitioned.then(|| (ProfileId("personal".into()), BindingId(binding.into()))),
        };
        Arc::new(Self {
            scope,
            binding,
            seen: Mutex::default(),
            sent: Mutex::default(),
        })
    }
}

impl Sender for BoundSender {
    fn scope(&self) -> DeliveryScope {
        self.scope.clone()
    }

    fn allows(&self, _: &Snapshot, item: &Outgoing) -> bool {
        self.seen.lock().unwrap().push(item.body.0.clone());
        self.binding == item.binding.0
    }

    fn send<'a>(
        &'a self,
        _: &'a Snapshot,
        item: &'a Outgoing,
        _: &'a ReplyBody,
    ) -> Pin<Box<dyn Future<Output = Delivery> + Send + 'a>> {
        self.sent.lock().unwrap().push(item.body.0.clone());
        Box::pin(async {
            Delivery::Sent {
                receipt: "receipt".into(),
            }
        })
    }
}

async fn reply(runs: &dyn RunStore, sender: &BoundSender) {
    let binding = sender.binding;
    let body = JsonPayload(serde_json::json!({"text": binding}).to_string());
    let run = runs
        .accept(Accept {
            reply_transport: Some(sender.scope.transport.clone()),
            event: Envelope {
                profile: ProfileId("personal".into()),
                request: RequestId(binding.into()),
                binding: BindingId(binding.into()),
                principal: PrincipalId("slack:home/U1".into()),
                conversation: Conversation {
                    kind: ConversationKind::Direct,
                    id: "D1".into(),
                    thread: None,
                },
                http: None,
                attachments: vec![],
                body: body.clone(),
            },
            approver: None,
            revision: Revision("r1".into()),
            target: Target::Agent {
                agent: AgentId("main".into()),
                instruction: binding.into(),
            },
            scopes: ["run".into()].into(),
        })
        .await
        .unwrap();
    let ctx = runs.claim().await.unwrap().unwrap();
    assert_eq!(ctx.attempt.run, run);
    runs.finish(
        ctx.attempt,
        Finish {
            result: body.clone(),
            answers: vec![body],
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn two_senders_get_only_their_own_reply_bodies() {
    exercise(false).await;
}

#[tokio::test]
async fn two_senders_for_one_transport_can_partition_by_binding() {
    exercise(true).await;
}

async fn exercise(partitioned: bool) {
    let Some(schema) = Schema::create() else {
        return;
    };
    let store = Arc::new(
        Store::connect(&schema.url(), Config::default())
            .await
            .unwrap(),
    );
    store.migrate().await.unwrap();
    store
        .replace(
            None,
            Revision("set-1".into()),
            BTreeMap::from([(ProfileId("personal".into()), Revision("r1".into()))]),
            vec![],
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let snapshot = profile("personal")
        .revision("r1")
        .binding(
            "desk",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .binding(
            "other",
            Adapter::SlackDm {
                workspace: "away".into(),
            },
            "main",
        )
        .role("owner", &["run"])
        .member("slack:home/U1", &["owner"])
        .policy_scopes(&["run"])
        .build();
    let host = r#"{"slack":{"workspaces":{"home":{"team_id":"T1","dm":true,"bot_token":"bot-token","app_token":"app-token"},"away":{"team_id":"T2","dm":true,"bot_token":"bot-token","app_token":"app-token"}}}}"#;
    let ingress = Arc::new(Ingress::new(
        TestCatalog::new([snapshot]),
        Arc::new(HostConfig::from_json(host).unwrap()),
        store.clone(),
        MemoryPrivate::new(),
    ));
    let other_transport = if partitioned {
        PushTransportId::SLACK
    } else {
        PushTransportId::new("test-push").unwrap()
    };
    let owner = BoundSender::new("desk", PushTransportId::SLACK, partitioned);
    let other = BoundSender::new("other", other_transport, partitioned);
    let owner_loop = Deliverer::new(store.clone(), ingress.clone(), owner.clone());
    let other_loop = Deliverer::new(store.clone(), ingress, other.clone());

    reply(&*store, &owner).await;
    assert_eq!(other_loop.tick().await.unwrap(), 0);
    assert!(other.seen.lock().unwrap().is_empty());
    reply(&*store, &other).await;
    let (first, second) = tokio::join!(owner_loop.tick(), other_loop.tick());
    assert_eq!((first.unwrap(), second.unwrap()), (1, 1));
    for sender in [&owner, &other] {
        let expected = [serde_json::json!({"text": sender.binding}).to_string()];
        assert_eq!(*sender.seen.lock().unwrap(), expected);
        assert_eq!(*sender.sent.lock().unwrap(), expected);
    }
    assert_eq!(
        schema.query("select count(*) from {s}.bot_outbox where state = 'confirmed'"),
        "2"
    );
    assert_eq!(
        schema.query("select count(*) from {s}.bot_outbox where state = 'rejected'"),
        "0"
    );
}
