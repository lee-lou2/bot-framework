//! Network test doubles: DNS resolver, echo TCP connector, and scripted model upstream.

use crate::{
    model::{ChunkSource, Upstream, UpstreamRequest, UpstreamResponse, UpstreamStream},
    proxy::{Connector, Duplex, Resolver},
};
use bot_core::port::{Error, Reply};
use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::Mutex,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

use super::lock;

#[derive(Default)]
pub struct FakeResolver {
    table: Mutex<HashMap<String, Vec<IpAddr>>>,
    calls: Mutex<Vec<String>>,
}

impl FakeResolver {
    pub fn new(entries: &[(&str, &[&str])]) -> Self {
        let table = entries
            .iter()
            .map(|(host, ips)| {
                (
                    host.to_string(),
                    ips.iter().map(|ip| ip.parse().unwrap()).collect(),
                )
            })
            .collect();
        Self {
            table: Mutex::new(table),
            calls: Mutex::default(),
        }
    }
    pub fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }
}

impl Resolver for FakeResolver {
    fn resolve(&self, host: &str) -> Reply<'_, Vec<IpAddr>> {
        lock(&self.calls).push(host.to_owned());
        let found = lock(&self.table).get(host).cloned();
        Box::pin(async move { found.ok_or(Error::Unavailable) })
    }
}

#[derive(Default)]
pub struct FakeConnector {
    addrs: Mutex<Vec<SocketAddr>>,
}

impl FakeConnector {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn addrs(&self) -> Vec<SocketAddr> {
        lock(&self.addrs).clone()
    }
}

impl Connector for FakeConnector {
    fn connect(&self, addr: SocketAddr) -> Reply<'_, Box<dyn Duplex>> {
        lock(&self.addrs).push(addr);
        let (near, mut far) = duplex(8192);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = far.read(&mut buf).await {
                if n == 0 || far.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });
        Box::pin(async move { Ok(Box::new(near) as Box<dyn Duplex>) })
    }
}

type Seen = (String, String, Vec<(String, String)>, String);

enum Step {
    Whole(u16, Vec<u8>),
    Fail,
    Hang,
    Chunks(u16, Vec<Vec<u8>>, End, Duration),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum End {
    Eof,
    Fail,
    Hang,
}

struct FakeChunks {
    chunks: VecDeque<Vec<u8>>,
    end: End,
    gap: Duration,
}

impl ChunkSource for FakeChunks {
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>> {
        if let Some(chunk) = self.chunks.pop_front() {
            let gap = self.gap;
            return Box::pin(async move {
                if !gap.is_zero() {
                    tokio::time::sleep(gap).await;
                }
                Ok(Some(chunk))
            });
        }
        let end = self.end;
        Box::pin(async move {
            match end {
                End::Eof => Ok(None),
                End::Fail => Err(Error::Unavailable),
                End::Hang => std::future::pending().await,
            }
        })
    }
}

/// Scripted upstream double; defaults to `200 {"ok":true}` when no response is queued.
#[derive(Default)]
pub struct FakeUpstream {
    requests: Mutex<Vec<Seen>>,
    script: Mutex<VecDeque<Step>>,
    delay: Mutex<Duration>,
}

impl FakeUpstream {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn respond(&self, status: u16, body: &[u8]) {
        lock(&self.script).push_back(Step::Whole(status, body.to_vec()));
    }
    pub fn respond_chunks(&self, status: u16, chunks: &[&[u8]], end: End) {
        self.respond_slowly(status, chunks, Duration::ZERO, end);
    }
    pub fn respond_slowly(&self, status: u16, chunks: &[&[u8]], gap: Duration, end: End) {
        lock(&self.script).push_back(Step::Chunks(
            status,
            chunks.iter().map(|c| c.to_vec()).collect(),
            end,
            gap,
        ));
    }
    pub fn fail(&self) {
        lock(&self.script).push_back(Step::Fail);
    }
    pub fn set_delay(&self, delay: Duration) {
        *lock(&self.delay) = delay;
    }
    pub fn hang(&self) {
        lock(&self.script).push_back(Step::Hang);
    }
    /// Recorded requests as `(host:port, path, headers, body)`.
    pub fn requests(&self) -> Vec<Seen> {
        lock(&self.requests).clone()
    }

    fn record(&self, request: UpstreamRequest) {
        lock(&self.requests).push((
            format!("{}:{}", request.endpoint.host, request.endpoint.port),
            request.path,
            request.headers,
            String::from_utf8_lossy(&request.body).into_owned(),
        ));
    }

    fn next_step(&self) -> Step {
        lock(&self.script)
            .pop_front()
            .unwrap_or_else(|| Step::Whole(200, b"{\"ok\":true}".to_vec()))
    }
}

impl Upstream for FakeUpstream {
    fn send(&self, request: UpstreamRequest) -> Reply<'_, UpstreamResponse> {
        self.record(request);
        let next = match self.next_step() {
            Step::Whole(status, body) => Ok(UpstreamResponse { status, body }),
            Step::Chunks(status, chunks, _, _) => Ok(UpstreamResponse {
                status,
                body: chunks.concat(),
            }),
            Step::Fail => Err(Error::Unavailable),
            Step::Hang => return Box::pin(std::future::pending()),
        };
        let delay = *lock(&self.delay);
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            next
        })
    }

    fn stream(&self, request: UpstreamRequest) -> Reply<'_, UpstreamStream> {
        self.record(request);
        let next = match self.next_step() {
            Step::Whole(status, body) => Ok(UpstreamStream {
                status,
                body: Box::new(FakeChunks {
                    chunks: VecDeque::from([body]),
                    end: End::Eof,
                    gap: Duration::ZERO,
                }),
            }),
            Step::Chunks(status, chunks, end, gap) => Ok(UpstreamStream {
                status,
                body: Box::new(FakeChunks {
                    chunks: chunks.into(),
                    end,
                    gap,
                }),
            }),
            Step::Fail => Err(Error::Unavailable),
            Step::Hang => return Box::pin(std::future::pending()),
        };
        let delay = *lock(&self.delay);
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            next
        })
    }
}
