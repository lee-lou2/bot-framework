//! `ProfileSource` 포트: root=BOT_PROFILES 전체 집합의 판본을 돌려준다.
mod support;

use bot_core::port::{Error, ProfileSource};
use bot_profile::{FsProfiles, Limits, load};
use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};
use support::{Temp, base, edit, host, write_profile};

/// 파일 읽기뿐이라 한 번 폴링하면 끝난다.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("이 포트는 기다리지 않는다"),
    }
}

fn source() -> FsProfiles {
    FsProfiles {
        host: host(),
        limits: Limits::default(),
    }
}

#[test]
fn validate_returns_the_set_revision() {
    let root = Temp::new("source");
    write_profile(root.path(), "personal", &base());
    let revision = block_on(source().validate(root.path())).unwrap();
    let loaded = load(root.path(), &host(), &Limits::default()).unwrap();
    assert_eq!(&revision, loaded.revision());
    assert_eq!(revision.0.len(), 64);
    // 한 바이트만 바뀌어도 집합 판본이 바뀐다
    write_profile(
        root.path(),
        "personal",
        &edit(base(), "rules.md", "한국어로", "영어로"),
    );
    assert_ne!(block_on(source().validate(root.path())).unwrap(), revision);
}

#[test]
fn validate_maps_failures_to_port_errors() {
    let missing = Temp::new("missing");
    let gone = missing.path().join("nope");
    assert!(matches!(
        block_on(source().validate(&gone)),
        Err(Error::Missing)
    ));
    let bad = Temp::new("bad");
    write_profile(
        bad.path(),
        "personal",
        &edit(base(), "profile.yaml", "version: 1", "version: 9"),
    );
    assert!(matches!(
        block_on(source().validate(bad.path())),
        Err(Error::Invalid)
    ));
    let empty = Temp::new("empty");
    assert!(matches!(
        block_on(source().validate(empty.path())),
        Err(Error::Invalid)
    ));
}
