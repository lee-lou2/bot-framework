//! `bot init`·`bot add`: 프로필 작업 폴더의 시작 파일을 만든다. 새 파일만 만들고(`files::Writer`: 덮어쓰지 않음·링크 따라가지 않음·
//! 중간 실패 시 되돌림) profile.yaml의 권한 상한은 읽지도 고치지도 않는다 — 권한은 안내만 출력한다. 런타임 보안 경계 밖의 도구다.
mod add;
mod files;
mod templates;

pub use add::{agent, skill};

use crate::cli::Template;
use bot_core::id::is_name;
use bot_profile::{Limits, Registry};
use files::Writer;
use std::path::Path;

fn label(template: Template) -> &'static str {
    match template {
        Template::Personal => "personal",
        Template::Team => "team",
        Template::Company => "company",
    }
}

/// 셸에서 그대로 붙여 넣을 수 있게 작은따옴표로 감싼다.
fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// `<dir>/<name>/`에 작업 폴더를 만든다: `host.json`(데모 모델·이 OS 사용자 `uid`의 CLI 계정)과 `profiles/<name>/`(템플릿).
/// 이미 있는 폴더·파일은 건드리지 않고 거절한다. 성공하면 사용자에게 보여 줄 안내 글을 돌려준다.
pub fn init(name: &str, template: Template, dir: &Path, uid: u32) -> Result<String, String> {
    if !is_name(name) {
        return Err(format!(
            "이름 {name:?}은(는) 소문자로 시작하는 소문자·숫자·하이픈(63자 이하)이어야 해요 — 예: my-bot"
        ));
    }
    let parent = Writer::new(dir)?;
    parent.require_absent(&[name])?;
    let planned = plan(name, templates::files(template), uid);
    let files = build(&parent, &std::env::temp_dir(), name, &planned)?;
    let base = std::path::absolute(dir.join(name)).unwrap_or_else(|_| dir.join(name));
    let mut lines = vec![format!(
        "만들었어요: {} (템플릿 {}, 에이전트 {}개)",
        base.display(),
        label(template),
        templates::agent_count(template)
    )];
    lines.extend(files.iter().map(|path| format!("  {path}")));
    lines.extend([
        "다음 단계:".to_owned(),
        format!("  export BOT_PROFILES={}", quote(&base.join("profiles"))),
        format!("  export BOT_HOST_CONFIG={}", quote(&base.join("host.json"))),
        "  bot validate   # 구조·호스트 점검(DB·Slack 접속 없음)".to_owned(),
        "  bot doctor     # DB·자격·모델 설정 점검(기본은 네트워크 접속 없음)".to_owned(),
        "host.json은 데모 모델과 지금 OS 사용자(CLI 계정 owner)만 담은 예시예요. 실제 모델·입구·도구는 호스트 설정에 직접 더해요."
            .to_owned(),
    ]);
    Ok(lines.join("\n"))
}

/// 템플릿 글의 자리표시자를 채운다(이름은 `is_name`을 통과한 값이다).
fn fill(text: &str, name: &str, uid: u32) -> String {
    text.replace("__ID__", name)
        .replace("__UID__", &uid.to_string())
}

/// 만들 파일: 호스트 설정 예시의 글과 (작업 폴더 안의 상대 경로, 내용). 검증하는 사본과 게시하는 폴더가 **이 같은 바이트**를 쓴다.
struct Planned {
    host: String,
    files: Vec<(String, String)>,
}

fn plan(name: &str, files: &[(&str, &str)], uid: u32) -> Planned {
    let host = fill(templates::HOST_JSON, name, uid);
    let mut planned = vec![("host.json".to_owned(), host.clone())];
    planned.extend(
        files
            .iter()
            .map(|(path, text)| (format!("profiles/{name}/{path}"), fill(text, name, uid))),
    );
    Planned {
        host,
        files: planned,
    }
}

fn write_all(writer: &mut Writer, planned: &Planned) -> Result<(), String> {
    for (path, text) in &planned.files {
        writer.write(path, text)?;
    }
    Ok(())
}

/// 검증은 사용자가 준 경로를 다시 열지 않는다(그 사이 바뀌었다면 게시할 것과 다른 것을 검증하게 된다): 같은 바이트를 `temp`(`$TMPDIR`) 아래
/// 우리가 만든 임시 사본(열자마자 소유자·권한·비어 있음을 확인한다)에 써서 `bot validate`와 같은 검증기(프로필 로더)로 읽고, 사본은 끝나면 치운다.
fn verify_copy(temp: &Path, planned: &Planned) -> Result<(), String> {
    let base = Writer::new(temp)
        .map_err(|error| format!("검증용 임시 폴더($TMPDIR)를 쓸 수 없어요: {error}"))?;
    let mut copy = base.stage()?;
    let checked = write_all(&mut copy.writer, planned)
        .and_then(|()| verify(&temp.join(copy.name()).join("profiles"), &planned.host));
    copy.discard();
    checked
}

/// 검증을 통과한 바이트를 열어 둔 `parent` 기준으로 임시 폴더에 쓰고 최종 이름으로 한 번에 옮긴다: 검증이 어긋나면 사용자의 폴더에는 아무것도
/// 만들지 않고, 쓰다 실패하면 우리가 만든 이름만 치우며, 최종 이름에는 아무것도 생기지 않는다(이미 있으면 옮기기가 거절한다). 만든 파일의
/// 경로(정렬)를 돌려준다.
fn build(
    parent: &Writer,
    temp: &Path,
    name: &str,
    planned: &Planned,
) -> Result<Vec<String>, String> {
    verify_copy(temp, planned)?;
    let mut stage = parent.stage()?;
    if let Err(error) = write_all(&mut stage.writer, planned) {
        stage.discard();
        return Err(error);
    }
    let created = stage.writer.created();
    stage.publish(name)?;
    Ok(created)
}

/// 만든 프로필 폴더를 `bot validate`와 같은 로더로 읽어 본다(호스트 설정은 방금 쓴 예시와 같은 글로 해석한다).
fn verify(profiles: &Path, host_text: &str) -> Result<(), String> {
    let host = crate::host::parse(host_text).map_err(|error| {
        format!("호스트 설정 예시가 올바르지 않아 아무것도 만들지 않았어요: {error}")
    })?;
    Registry::new()
        .reload(profiles, &host.catalog, &Limits::default())
        .map(|_| ())
        .map_err(|report| {
            format!("프로필이 검증을 통과하지 못해 아무것도 만들지 않았어요:\n{report}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn count(path: &Path) -> usize {
        fs::read_dir(path).unwrap().count()
    }

    /// 막음 7: 이름 규칙에 맞는 `null`·`true`도 YAML에서 글로 읽히고, 어긋난 프로필은 검증기가 잡아 낸다.
    #[test]
    fn names_that_yaml_reads_as_non_text_still_make_a_valid_profile() {
        for name in ["null", "true", "false", "yes", "no", "on", "off", "y", "n"] {
            for template in [Template::Personal, Template::Team, Template::Company] {
                let dir = tempfile::tempdir().unwrap();
                init(name, template, dir.path(), 1000)
                    .unwrap_or_else(|error| panic!("{name} {template:?}: {error}"));
            }
        }
        // 검증기가 거절하는 프로필(따옴표 없는 `id: null`)은 사용자의 폴더에 아무것도 만들지 않고, 검증용 사본도 남지 않는다
        let (dir, temp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut planned = plan("null", templates::files(Template::Personal), 1000);
        for (_, text) in &mut planned.files {
            *text = text.replace("id: \"null\"", "id: null");
        }
        let parent = Writer::new(dir.path()).unwrap();
        let error = build(&parent, temp.path(), "null", &planned).unwrap_err();
        assert!(error.contains("검증을 통과하지 못해"), "{error}");
        assert_eq!(count(dir.path()), 0, "사용자의 폴더는 그대로다: {error}");
        assert_eq!(count(temp.path()), 0, "검증용 사본은 치웠다: {error}");
    }

    /// 막음 1: 검증은 사용자 경로가 아니라 `temp` 아래 사본에서 하고, 통과하면 같은 바이트가 게시되며 사본은 남지 않는다.
    #[test]
    fn the_verified_bytes_are_the_published_bytes_and_the_private_copy_is_removed() {
        let (dir, temp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let planned = plan("demo", templates::files(Template::Team), 1000);
        let parent = Writer::new(dir.path()).unwrap();
        let created = build(&parent, temp.path(), "demo", &planned).unwrap();
        assert_eq!(count(temp.path()), 0, "검증용 사본은 치웠다");
        assert_eq!(count(dir.path()), 1, "최종 이름 하나만 남는다");
        assert_eq!(created.len(), planned.files.len());
        for (path, text) in &planned.files {
            let published = fs::read_to_string(dir.path().join("demo").join(path)).unwrap();
            assert_eq!(&published, text, "{path}");
        }
        // 쓸 수 없는 임시 폴더는 사용자의 폴더에 아무것도 만들기 전에 알린다
        let gone = temp.path().join("not-here");
        let error = build(&parent, &gone, "other", &planned).unwrap_err();
        assert!(error.contains("임시 폴더($TMPDIR)"), "{error}");
        assert_eq!(count(dir.path()), 1);
    }
}
