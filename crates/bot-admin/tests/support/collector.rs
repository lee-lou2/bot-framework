//! Mock OTLP HTTP collector with an independent protobuf wire decoder.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Ok,
    Status(u16),
    /// Accepts connections and never responds.
    Hang,
    /// Closes accepted connections immediately.
    Reset,
    /// Records the request, sleeps for the duration, then replies 200.
    Late(Duration),
}

#[derive(Clone, Debug)]
pub struct Req {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct Collector {
    pub addr: SocketAddr,
    mode: Arc<Mutex<Mode>>,
    reqs: Arc<Mutex<Vec<Req>>>,
}

impl Collector {
    pub fn url(&self) -> String {
        format!("http://{}/v1/traces", self.addr)
    }

    pub fn url_at(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn set(&self, m: Mode) {
        *self.mode.lock().expect("lock") = m;
    }

    pub fn requests(&self) -> Vec<Req> {
        self.reqs.lock().expect("lock").clone()
    }

    pub fn spans(&self) -> Vec<Span> {
        self.requests()
            .iter()
            .flat_map(|r| decode(&r.body))
            .collect()
    }

    pub fn logs(&self) -> Vec<LogRec> {
        self.requests()
            .iter()
            .flat_map(|r| decode_logs(&r.body))
            .collect()
    }
}

pub async fn start(mode: Mode) -> Collector {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (mode, reqs) = (Arc::new(Mutex::new(mode)), Arc::new(Mutex::new(Vec::new())));
    let (m, r) = (mode.clone(), reqs.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let (m, r) = (m.clone(), r.clone());
            tokio::spawn(async move {
                let current = *m.lock().expect("lock");
                if matches!(current, Mode::Reset) {
                    return;
                }
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.lines();
                let path = lines
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or_default()
                    .to_owned();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let want: usize = headers
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + want {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                if matches!(current, Mode::Hang) {
                    std::future::pending::<()>().await;
                }
                let code = match current {
                    Mode::Status(c) => c,
                    _ => 200,
                };
                if code < 300 {
                    r.lock().expect("lock").push(Req {
                        path,
                        headers,
                        body: buf[head_end..head_end + want].to_vec(),
                    });
                }
                if let Mode::Late(wait) = current {
                    tokio::time::sleep(wait).await;
                }
                let resp =
                    format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    Collector { addr, mode, reqs }
}

// Independent protobuf wire decoder.

enum Wire {
    Varint(u64),
    Fixed64(u64),
    Len(Vec<u8>),
}

fn varint(b: &[u8]) -> (u64, usize) {
    let (mut v, mut shift) = (0u64, 0);
    for (i, byte) in b.iter().enumerate() {
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return (v, i + 1);
        }
        shift += 7;
    }
    panic!("truncated varint");
}

fn parse(mut b: &[u8]) -> Vec<(u32, Wire)> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let (key, n) = varint(b);
        b = &b[n..];
        let field = u32::try_from(key >> 3).expect("field number");
        match key & 7 {
            0 => {
                let (v, n) = varint(b);
                b = &b[n..];
                out.push((field, Wire::Varint(v)));
            }
            1 => {
                out.push((
                    field,
                    Wire::Fixed64(u64::from_le_bytes(b[..8].try_into().expect("8 bytes"))),
                ));
                b = &b[8..];
            }
            2 => {
                let (len, n) = varint(b);
                b = &b[n..];
                let len = usize::try_from(len).expect("length");
                out.push((field, Wire::Len(b[..len].to_vec())));
                b = &b[len..];
            }
            other => panic!("unknown wire type {other}"),
        }
    }
    out
}

fn lens(f: &[(u32, Wire)], n: u32) -> Vec<&[u8]> {
    f.iter()
        .filter(|(k, _)| *k == n)
        .filter_map(|(_, w)| {
            if let Wire::Len(v) = w {
                Some(v.as_slice())
            } else {
                None
            }
        })
        .collect()
}

fn string(f: &[(u32, Wire)], n: u32) -> String {
    lens(f, n)
        .first()
        .map(|b| String::from_utf8(b.to_vec()).expect("UTF-8"))
        .unwrap_or_default()
}

fn num(f: &[(u32, Wire)], n: u32) -> u64 {
    f.iter()
        .find_map(|(k, w)| {
            if *k == n {
                if let Wire::Varint(v) | Wire::Fixed64(v) = w {
                    Some(*v)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Attr {
    Str(String),
    Int(i64),
    Bool(bool),
}

fn key_values(f: &[(u32, Wire)], n: u32) -> BTreeMap<String, Attr> {
    lens(f, n)
        .into_iter()
        .map(|kv| {
            let kv = parse(kv);
            let v = parse(lens(&kv, 2).first().copied().unwrap_or_default());
            let value = match v.first() {
                Some((1, Wire::Len(s))) => Attr::Str(String::from_utf8(s.clone()).expect("UTF-8")),
                Some((2, Wire::Varint(b))) => Attr::Bool(*b != 0),
                Some((3, Wire::Varint(i))) => Attr::Int(*i as i64),
                other => panic!("unknown attr value present={}", other.is_some()),
            };
            (string(&kv, 1), value)
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Span {
    pub resource: BTreeMap<String, Attr>,
    pub scope: (String, String),
    pub trace: Vec<u8>,
    pub span: Vec<u8>,
    pub parent: Vec<u8>,
    pub name: String,
    pub kind: u64,
    pub start: u64,
    pub end: u64,
    pub attrs: BTreeMap<String, Attr>,
    pub events: Vec<(String, u64, BTreeMap<String, Attr>)>,
    pub links: Vec<(Vec<u8>, Vec<u8>)>,
    pub status: (u64, String),
}

impl Span {
    pub fn str(&self, k: &str) -> Option<&str> {
        match self.attrs.get(k) {
            Some(Attr::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn int(&self, k: &str) -> Option<i64> {
        match self.attrs.get(k) {
            Some(Attr::Int(n)) => Some(*n),
            _ => None,
        }
    }
}

pub fn decode(body: &[u8]) -> Vec<Span> {
    let mut out = Vec::new();
    for rs in lens(&parse(body), 1) {
        let rs = parse(rs);
        let resource = lens(&rs, 1)
            .first()
            .map(|r| key_values(&parse(r), 1))
            .unwrap_or_default();
        for ss in lens(&rs, 2) {
            let ss = parse(ss);
            let sc = lens(&ss, 1).first().map(|s| parse(s)).unwrap_or_default();
            let scope = (string(&sc, 1), string(&sc, 2));
            for sp in lens(&ss, 2) {
                let f = parse(sp);
                let status = lens(&f, 15).first().map(|s| parse(s)).unwrap_or_default();
                out.push(Span {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    trace: lens(&f, 1).first().map(|b| b.to_vec()).unwrap_or_default(),
                    span: lens(&f, 2).first().map(|b| b.to_vec()).unwrap_or_default(),
                    parent: lens(&f, 4).first().map(|b| b.to_vec()).unwrap_or_default(),
                    name: string(&f, 5),
                    kind: num(&f, 6),
                    start: num(&f, 7),
                    end: num(&f, 8),
                    attrs: key_values(&f, 9),
                    events: lens(&f, 11)
                        .into_iter()
                        .map(|e| {
                            let e = parse(e);
                            (string(&e, 2), num(&e, 1), key_values(&e, 3))
                        })
                        .collect(),
                    links: lens(&f, 13)
                        .into_iter()
                        .map(|l| {
                            let l = parse(l);
                            (lens(&l, 1)[0].to_vec(), lens(&l, 2)[0].to_vec())
                        })
                        .collect(),
                    status: (num(&status, 3), string(&status, 2)),
                });
            }
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct LogRec {
    pub resource: BTreeMap<String, Attr>,
    pub scope: (String, String),
    pub time_nano: u64,
    pub severity_number: u64,
    pub severity_text: String,
    pub body: String,
    pub attrs: BTreeMap<String, Attr>,
    pub dropped: u64,
    pub trace: Vec<u8>,
    pub span: Vec<u8>,
}

pub fn decode_logs(body: &[u8]) -> Vec<LogRec> {
    let mut out = Vec::new();
    for rl in lens(&parse(body), 1) {
        let rl = parse(rl);
        let resource = lens(&rl, 1)
            .first()
            .map(|r| key_values(&parse(r), 1))
            .unwrap_or_default();
        for sl in lens(&rl, 2) {
            let sl = parse(sl);
            let sc = lens(&sl, 1).first().map(|s| parse(s)).unwrap_or_default();
            let scope = (string(&sc, 1), string(&sc, 2));
            for lr in lens(&sl, 2) {
                let f = parse(lr);
                let body_val = lens(&f, 5)
                    .first()
                    .map(|b| string(&parse(b), 1))
                    .unwrap_or_default();
                out.push(LogRec {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    time_nano: num(&f, 1),
                    severity_number: num(&f, 2),
                    severity_text: string(&f, 3),
                    body: body_val,
                    attrs: key_values(&f, 6),
                    dropped: num(&f, 7),
                    trace: lens(&f, 9).first().map(|b| b.to_vec()).unwrap_or_default(),
                    span: lens(&f, 10).first().map(|b| b.to_vec()).unwrap_or_default(),
                });
            }
        }
    }
    out
}
