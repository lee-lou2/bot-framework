//! Canonical argument serialization and SHA-256 sealing (`CreateSeal`) for `schedule.create` approval cards.

use super::{MAX_INSTRUCTION_BYTES, ScheduleSpec, Timing, utc};
use crate::{canon, port::Error};
use serde_json::{Value, json};

pub struct CreateSeal {
    pub canonical: String,
    pub sha256: [u8; 32],
}

/// Validates canonical IANA timezone names (shared by profile validation, sealing, and the store).
pub fn valid_iana_timezone(name: &str) -> bool {
    jiff_tzdb::get(name).is_some_and(|(canon, _)| canon == name)
}

impl ScheduleSpec {
    /// Deterministic (never reads wall clock) so redeliveries produce the same digest;
    /// callers must fill omitted timezones from the pinned profile first.
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
                if !valid_iana_timezone(timezone) {
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
