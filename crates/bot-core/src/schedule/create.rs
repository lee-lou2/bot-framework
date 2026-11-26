//! `schedule.create` 승인 카드에 묶는 인자의 정규 표현과 SHA-256(`CreateSeal`).

use super::{MAX_INSTRUCTION_BYTES, ScheduleSpec, Timing, utc};
use crate::{canon, port::Error};
use serde_json::{Value, json};

/// 카드에 표시할 schedule.create 인자와 그 UTF-8 바이트의 SHA-256.
pub struct CreateSeal {
    pub canonical: String,
    pub sha256: [u8; 32],
}

impl ScheduleSpec {
    /// 현재 시각을 읽지 않는다. 완료 호출의 재전달에서도 같은 지문을 만든다.
    /// 호출자는 생략된 시간대를 요청이 고정한 프로필 판본의 값으로 먼저 채운다.
    pub fn seal_create(&self) -> Result<CreateSeal, Error> {
        let Self {
            timing,
            instruction,
            quiet,
            destination,
        } = self;
        if instruction.trim().is_empty()
            || instruction.len() > MAX_INSTRUCTION_BYTES
            || instruction.contains('\0')
        {
            return Err(Error::Invalid);
        }
        let mut args = match timing {
            Timing::At(at) => json!({"at": utc::schedule_instant(*at).ok_or(Error::Invalid)?}),
            Timing::Cron {
                expression,
                timezone,
            } => {
                if jiff_tzdb::get(timezone).is_none_or(|(name, _)| name != timezone) {
                    return Err(Error::Invalid);
                }
                json!({"cron": expression.as_str(), "timezone": timezone})
            }
        };
        args["instruction"] = Value::String(instruction.clone());
        args["quiet"] = Value::Bool(*quiet);
        if let Some(target) = destination {
            args["destination"] = target.approval_target()?;
        }
        let canonical = canon::canonicalize(&args).ok_or(Error::Invalid)?;
        Ok(CreateSeal {
            sha256: canon::sha256(&canonical),
            canonical,
        })
    }
}

#[cfg(test)]
mod tests;
