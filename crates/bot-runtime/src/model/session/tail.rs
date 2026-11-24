//! 실행 중인 CLI의 세션 파일을 따라 읽는다(폴링). 줄을 `Journal`에 먹이고, 프런트가 완료한 응답의 도구 목록(`cli-resp`)이 세션 줄과 정확히
//! 맞으면 그 응답의 묶음을 `checkpoint_batch`로 한 번에 쓴다. 이 쓰기가 끝나야 프런트가 그 응답의 도구 호출을 실행한다.

use super::{
    Journal, MAX_LINE_BYTES, Plan, Refused, Session, SessionError, Why,
    open::{Opened, open_beneath},
};
use crate::model::ModelError;
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    time::{Duration, Instant},
};

/// 한 번에 읽는 최대 바이트(너무 큰 줄 하나가 틱을 막지 않게 끊어 읽는다)
const READ_CHUNK: usize = 4 << 20;
/// 프런트의 확인을 기다리는 시간 상한. 넘으면 이 세션을 믿을 수 없다(프런트도 같은 시간 안에 그룹을 기다린다).
const CONFIRM_WITHIN: Duration = Duration::from_secs(30);
/// 확인(원장 읽기)을 다시 시도하는 최소 간격
const PROBE_EVERY: Duration = Duration::from_millis(40);
/// 끝난 뒤 파일 끝까지 읽는 구간 수의 상한(구간마다 `READ_CHUNK`). 넘게 남으면 다 볼 수 없는 기록이다.
const FINISH_CHUNKS: usize = 16;

/// 한 번 읽은 결과. 읽지 못한 것(`Failed`)은 정상 끝(`Eof`)이 아니다.
enum Pull {
    Progress,
    Eof,
    Failed,
}

pub struct Tail {
    /// 작업 폴더와 그 안의 세션 파일 상대 경로. VM이 쓰는 곳이라 열 때마다 폴더에서 `openat`로 내려간다
    root: PathBuf,
    relative: String,
    offset: u64,
    partial: Vec<u8>,
    /// 줄 하나가 상한을 넘었다: 다음 개행까지 버린다
    skipping: bool,
    journal: Journal,
    port: Session,
    since: BTreeMap<String, Instant>,
    probed: Option<Instant>,
}

fn failed(code: &'static str, transient: bool) -> ModelError {
    ModelError::Failed { code, transient }
}

fn from_port(error: SessionError) -> Option<ModelError> {
    match error {
        SessionError::Lost => Some(ModelError::Lost),
        SessionError::Permanent(code) => Some(failed(code, false)),
        SessionError::Unavailable => None,
    }
}

impl Tail {
    /// `offset`부터 읽는다(미리 써 둔 복원 파일의 길이).
    pub fn new(
        root: PathBuf,
        relative: String,
        offset: u64,
        journal: Journal,
        port: Session,
    ) -> Self {
        Self {
            root,
            relative,
            offset,
            partial: Vec::new(),
            skipping: false,
            journal,
            port,
            since: BTreeMap::new(),
            probed: None,
        }
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// 파일에 새로 생긴 줄을 먹인다. 경로가 일반 파일이 아니면(링크·FIFO·폴더 교체) 이 세션을 믿을 수 없다.
    fn read_new(&mut self) -> Result<Pull, ModelError> {
        let mut file = match open_beneath(&self.root, &self.relative) {
            Opened::File(file) => file,
            Opened::Missing => return Ok(Pull::Eof),
            Opened::Unsafe => return Err(failed("session_unsafe", false)),
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Ok(Pull::Failed);
        }
        let mut chunk = Vec::new();
        let Ok(read) = file.take(READ_CHUNK as u64).read_to_end(&mut chunk) else {
            return Ok(Pull::Failed);
        };
        self.offset += read as u64;
        self.partial.extend_from_slice(&chunk);
        while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            if std::mem::take(&mut self.skipping) {
                continue; // 상한을 넘은 줄의 나머지
            }
            if let Ok(text) = std::str::from_utf8(&line[..line.len() - 1])
                && !text.trim().is_empty()
            {
                self.journal.feed(text);
            }
        }
        // 개행 없이 상한을 넘는 줄은 쌓아 두지 않는다: 이 세션은 더 쓰지 않고 그 줄의 나머지를 버린다
        if self.partial.len() > MAX_LINE_BYTES {
            self.journal.freeze(Why::TooLarge);
            self.partial.clear();
            self.skipping = true;
        } else if self.skipping {
            self.partial.clear();
        }
        Ok(if read == 0 { Pull::Eof } else { Pull::Progress })
    }

    /// 확인된 응답의 묶음을 쓴다. 쓸 수 없으면 다음 틱에 다시(`Ok(false)`) 한다.
    async fn write(&mut self, plan: Plan) -> Result<bool, ModelError> {
        for slot in &plan.verify {
            match self.port.0.recall(slot).await {
                Ok(Some(_)) => {}
                // 끝난 호출의 결과가 아니다(자리표시): 이 세션은 더 쓰지 않는다. 재개 때 호스트가 실제 결과로 채운다
                Ok(None) => {
                    self.journal.freeze(Why::Unsaved);
                    return Ok(true);
                }
                Err(error) => return from_port(error).map_or(Ok(false), Err),
            }
        }
        match self.port.0.commit(plan.steps.clone()).await {
            Ok(()) => {
                self.journal.applied(&plan);
                Ok(true)
            }
            Err(error) => from_port(error).map_or(Ok(false), Err),
        }
    }

    /// 개행 없이 끝난 마지막 기록도 하나의 기록이다: 온전한 JSON이면 먹이고, 잘렸으면 정상 끝이 아니다(그 안에 호출이 있었는지 알 수 없다).
    fn last_record(&mut self) -> Result<(), ModelError> {
        if self.skipping || self.partial.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let text = std::str::from_utf8(&self.partial)
            .ok()
            .filter(|t| serde_json::from_str::<serde_json::Value>(t).is_ok())
            .map(str::to_owned)
            .ok_or_else(|| failed("session_unsaved", true))?;
        self.partial.clear();
        self.journal.feed(&text);
        Ok(())
    }

    /// 얼린 채로 릴레이 도구 호출이 막혔으면 이 CLI는 더 도구를 쓰지 못한다(프런트는 저장되지 않은 그룹을 실행하지 않는다): 끝내고 실패로 알린다.
    /// 재개하면 호스트가 끝나지 않은 자리를 채워 이어 가므로 고칠 수 있는 사유(`Unsaved`)는 다시 시도하고, 상한은 다시 해도 같으니 멈춘다.
    fn stuck(&self) -> Result<(), ModelError> {
        match self.journal.stuck() {
            Some(Why::Unsaved) => Err(failed("session_unsaved", true)),
            Some(Why::TooLarge) => Err(failed("session_too_large", false)),
            None => Ok(()),
        }
    }

    /// 한 번 훑는다: 새 줄을 읽고, 확인을 기다리는 가장 오래된 응답부터 묶음을 쓴다.
    pub async fn poll(&mut self) -> Result<(), ModelError> {
        self.pump().await?;
        self.stuck()
    }

    /// CLI가 끝난 뒤 최종 답을 올리기 전의 마지막 확인. 한 번에 읽는 양보다 큰 기록도 파일 끝까지 구간별로 읽고(한도 안에서 끝까지 못 보면 닫는다),
    /// 저장하지 못한 릴레이 응답이 남았거나 이 claim이 저장한 릴레이 호출 중 원장에서 끝나지 않은 것(일시 실패로 실행되지 못했다)이 있으면
    /// 성공으로 끝내지 않는다. 호출한 쪽이 전체 시간 안에서 부른다.
    pub async fn finish(&mut self) -> Result<(), ModelError> {
        let mut drained = false;
        for _ in 0..=FINISH_CHUNKS {
            match self.read_new()? {
                Pull::Progress => tokio::task::yield_now().await, // 구간 사이에 취소·마감이 끼어들 틈을 준다
                Pull::Eof => {
                    drained = true;
                    break;
                }
                // 읽지 못한 것은 끝난 것이 아니다: 이 세션의 뒤를 본 적이 없다
                Pull::Failed => return Err(failed("session_unreadable", true)),
            }
        }
        if !drained {
            return Err(failed("session_too_large", false));
        }
        self.last_record()?;
        self.stuck()?;
        if self.journal.unsaved_relay() {
            return Err(failed("session_unsaved", true));
        }
        for slot in self.journal.saved_relay() {
            match self.port.0.ended(slot).await {
                Ok(true) => {}
                // 계획은 저장됐지만 호출이 실행되지 못했다(프런트가 저장을 기다리다 실패시켰거나 일시 실패): 재개가 호스트 쪽에서 마무리한다
                Ok(false) => return Err(failed("session_unsaved", true)),
                Err(error) => {
                    return Err(from_port(error).unwrap_or(failed("session_unavailable", true)));
                }
            }
        }
        Ok(())
    }

    async fn pump(&mut self) -> Result<(), ModelError> {
        self.read_new()?;
        while let Some(id) = self.journal.oldest().map(str::to_owned) {
            let first = *self.since.entry(id.clone()).or_insert_with(Instant::now);
            if self.probed.is_some_and(|at| at.elapsed() < PROBE_EVERY) {
                return Ok(());
            }
            self.probed = Some(Instant::now());
            let waiting = match self.port.0.response(&id).await {
                Ok(Some(response)) => match self.journal.plan(&response) {
                    Ok(plan) => {
                        if self.write(plan).await? {
                            self.since.remove(&id);
                            self.probed = None;
                            continue; // 다음 응답이 이미 도착해 있을 수 있다
                        }
                        true
                    }
                    Err(Refused::Incomplete | Refused::NotYet) => true,
                    Err(Refused::Mismatch | Refused::Unverified) => {
                        return Err(failed("session_mismatch", false));
                    }
                },
                Ok(None) => true,
                Err(error) => from_port(error).map_or(Ok(true), Err)?,
            };
            if waiting && first.elapsed() > CONFIRM_WITHIN {
                return Err(failed("response_unconfirmed", true));
            }
            return Ok(());
        }
        Ok(())
    }
}
