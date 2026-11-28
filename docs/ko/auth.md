# 인증, OAuth 및 관리형 자격 증명

[English](../auth.md) | 한국어

호스트 프로세스는 모든 모델과 원격 MCP 자격 증명을 단일 `AuthResolver` 포트(`crates/bot-core/src/auth.rs`)로 해석합니다. 모델, 샌드박스, VM은 API 키, OAuth 액세스/리프레시 토큰, 서비스 계정 개인 키를 절대 볼 수 없습니다.

## 저장소와 암호화 (`bot_credentials`)

정적 시크릿은 기존처럼 `SecretStore`(`keychain`, `sops:<file>`, `env`)에 보관합니다. 갱신되는 자격 증명이나 호스트 관리 자격 증명(`oauth`, `service-account`, `subscription-token`)은 호스트에서 **AES-256-GCM**(`ring`)으로 암호화해 PostgreSQL(`bot_credentials`, 마이그레이션 `0030_credentials.sql`)에 저장하므로 다중 워커 Linux·macOS 배포에서도 단일 영속 원본을 공유합니다(각 워커의 캐시는 30초마다 PostgreSQL 메타데이터를 재검증해 재시작 없이 `bot auth logout`, 회전, `needs_login`을 반영합니다).

- **마스터 키:** 32바이트 임의 키(64자리 16진수)를 생성해 `SecretStore`의 `host` 프로필 아래 예약 시크릿 이름 `credential-key`(`bot/host/credential-key`)에 저장합니다. `bot validate`와 `bot doctor`는 프로바이더나 원격 MCP 서버가 관리형 `auth`를 참조할 때 이 키의 존재를 자동으로 검증합니다.
- **AAD 바인딩과 egress 상한:** 모든 암호문은 매 쓰기마다 새 96비트 nonce와 함께 `(profile, name, kind, token_endpoint_host:port)`에 인증 바인딩됩니다. 다른 프로필·이름·종류·토큰 엔드포인트로 암호문을 옮기면 복호화에 실패합니다. 모든 OAuth 탐색·등록·토큰 엔드포인트는 명시적 `egress.<profile>` `host:port` 허용 항목이 필수입니다(누락 시 차단). 기존 자격 증명의 토큰 엔드포인트 호스트나 OAuth issuer를 바꾸려면 먼저 명시적으로 `bot auth logout`을 실행해야 합니다.
- **단일 비행 갱신과 크래시 안전성:** 만료가 임박한 토큰은 행 단위 권고 락(`pg_try_advisory_lock`)을 잡은 전용 PostgreSQL 연결 안에서 갱신합니다. 호스트는 행을 `refreshing`으로 표시하기 전과 실제 아웃바운드 토큰 POST 직전에 활성 리비전(`ActiveCheck`)을 이중 검증합니다. 영구 토큰 폐기 오류(`invalid_grant`, `refresh_token_reused`, `refresh_token_expired`, `refresh_token_invalidated`) 또는 프로세스 크래시로 회전 중인 OAuth 갱신이 중단된 경우에만 운영자가 다시 로그인할 때까지 행을 `needs_login`(`credential_needs_login`)으로 격리합니다. 일시적 `5xx`/`429`/네트워크/클라이언트 설정 오류는 짧은 쿨다운과 함께 `active`로 복구합니다. 분리된(detached) 갱신 태스크는 정상 종료 시 커밋을 마칠 때까지 대기합니다.
- **마스킹:** 로드되거나 새로 발급된 액세스 토큰, 리프레시 토큰, 클라이언트 시크릿, JWT 어서션은 아웃바운드 전송 전에 라이브 `Redactor`에 등록되며 업스트림 응답 유출 검사에도 사용됩니다.

## 지원 인증 모드 및 표면 정책

| 모드 | `host.json` `auth.kind` | 허용 표면 | 비고 |
|---|---|---|---|
| 정적 API 키 / Bearer | *(`secret` 사용)* | 직접 API, VM CLI 릴레이, 원격 MCP | 기존 `SecretStore` 경로; 활성화 지문 불변 |
| 원격 MCP OAuth 2.0 | `oauth` | 원격 HTTP MCP (`servers.<alias>.auth`) | RFC 9728 + RFC 8414 + RFC 7591 + PKCE S256; 선택적 고정 `issuer` 승인 바인딩; 전송 전 `401` 강제 갱신 |
| 운영자 소유 OAuth 2.0 | `oauth` | 직접 API, VM CLI 릴레이, 원격 MCP | 표준 RFC 6749 갱신(`form` 또는 `json` 본문), HTTPS + 명시적 `egress.<profile>` 필수 |
| Google 서비스 계정 *(실험적)* | `service-account` | 직접 API, VM CLI 릴레이(`agy-cli` 등), 원격 MCP | 비암호화 PKCS#8 RSA 키로 로컬 RS256 JWT-bearer 서명; 로컬 PG/토큰 서버 하네스 검증(실제 Google 업스트림 미검증) |
| 옵트인 Claude 구독 *(실험적)* | `subscription-token` | `claude-cli` VM 릴레이 **전용** (`api.anthropic.com`) | `allow_subscription_auth.<profile>: ["claude"]` 필수; `anthropic-beta`에 `oauth-2025-04-20` 병합; `anthropic-api` 및 제3자 호스트 거부; 실제 업스트림 미검증 |
| 옵트인 Codex 구독 *(실험적)* | `oauth` *(가져오기)* | `codex-cli` VM 릴레이 **전용** (`chatgpt.com`) | `allow_subscription_auth.<profile>: ["codex"]` 필수; 전용 `CODEX_HOME` 가져오기(`egress.<profile>`에 `auth.openai.com:443` 필수); 라이브 `~/.codex/auth.json`, `openai-api` 및 제3자 호스트 거부; 실제 업스트림 미검증 |
| Google/Antigravity 개인 구독 OAuth | *(미지원)* | 거부 | Google 이용약관 위반 제재 위험으로 의도적 제외; API 키 또는 `service-account` 사용 |

## `host.json` 설정

### 원격 MCP 서버 OAuth

```json
{
  "egress": {
    "personal": ["mcp.linear.app:443"]
  },
  "servers": {
    "linear": {
      "transport": "http",
      "url": "https://mcp.linear.app/mcp",
      "auth": {
        "kind": "oauth",
        "credential": "linear-oauth",
        "issuer": "https://mcp.linear.app"
      }
    }
  }
}
```

### 모델 프로바이더 (`auth` vs `secret`)

`claude-cli`, `codex-cli`, `agy-cli`, `anthropic-api`, `openai-api`에서는 `secret`과 `auth`를 함께 지정할 수 없습니다. 선택적 `max_tokens`(`1..=1000000`, 활성화 지문에서 제외되는 운영 튜닝)는 직접 API 프로바이더와 함께 `claude-cli`에서도 지원합니다.

```json
{
  "allow_subscription_auth": {
    "personal": ["claude", "codex"]
  },
  "providers": {
    "claude": {
      "kind": "claude-cli",
      "model": "claude-sonnet-4-6",
      "endpoint": "https://api.anthropic.com",
      "auth": {"kind": "subscription-token", "credential": "claude-sub"},
      "max_tokens": 16384
    },
    "codex": {
      "kind": "codex-cli",
      "model": "gpt-5.4",
      "endpoint": "https://chatgpt.com/backend-api/codex",
      "auth": {"kind": "oauth", "credential": "codex-sub"}
    },
    "agy": {
      "kind": "agy-cli",
      "model": "gemini-3.1-pro-preview",
      "endpoint": "https://generativelanguage.googleapis.com",
      "auth": {"kind": "service-account", "credential": "gcp-sa"}
    }
  }
}
```

- `allow_subscription_auth`는 `"claude"`와 `"codex"`만 허용하는 프로필별 옵트인 맵입니다. 비어 있지 않으면 활성화 지문에 포함되며(`bot serve --activate` 필요) 시작 시 고정 경고를 기록합니다.
- `anthropic-api`와 `openai-api`는 `auth.kind`로 `"oauth"` 또는 `"service-account"`만 허용하며 `"subscription-token"`, 가져온 Codex 구독 자격 증명, `sk-ant-oat…` 정적 키는 `credential_denied`로 거부합니다.

## `bot auth` 명령

모든 `bot auth` 하위 명령은 `BOT_DATABASE_URL`에 연결해 `BOT_SECRETS`에서 `host/credential-key`를 읽으며, 시크릿 값은 절대 출력하지 않습니다. 표준 입력으로 시크릿을 받는 명령은 대화형 TTY를 거부합니다.

| 명령 | 용도 및 플래그 |
|---|---|
| `bot auth login` | PKCE S256 브라우저 로그인. `--profile <p> --server <alias>`(RFC 9728 + RFC 8414 + RFC 7591 자동 탐색) 또는 `--profile <p> --name <n> --authorize-url <url> --token-url <url>` 사용. 선택: `--client-id <id>`, `--scope <s>`, `--paste`(루프백 리스너 대신 리다이렉트 URL 붙여넣기). |
| `bot auth set-oauth` | 표준 입력 파이프로 운영자 소유 OAuth JSON 번들(`{access_token, refresh_token?, expires_in?, client_secret?}`) 저장: `--profile <p> --name <n> --token-endpoint <url> --client-id <id>`, 선택 `--scope <s>`, 반복 가능 `--header Name=Value`(허용 목록: `chatgpt-account-id`, `openai-organization`, `x-goog-user-project`), `--json-body`. |
| `bot auth set-service-account` | Google 서비스 계정 JSON 키(비암호화 PKCS#8 RSA) 검증 및 저장: `--profile <p> --name <n> --file <path>`, 선택 `--scope <scope>`(기본값 `https://www.googleapis.com/auth/cloud-platform`). |
| `bot auth set-token` | 표준 입력 파이프로 옵트인 Claude setup-token(`claude setup-token`의 `sk-ant-oat…`) 저장: `--profile <p> --name <n> --provider claude`. |
| `bot auth import-codex` | 전용 Codex OAuth 로그인(`CODEX_HOME=<dedicated-dir> codex login --device-auth`) 가져오기: `--profile <p> --name <n> --from <path>`. 일반 파일(`O_NOFOLLOW`, `nlink == 1`, 현재 UID 소유, `0600` 권한)을 검증하고 라이브 `~/.codex/auth.json`과 `$CODEX_HOME/auth.json`을 거부하며, `id_token`/`OPENAI_API_KEY`를 폐기하고 `--keep-source`가 없으면 원본 파일을 삭제합니다. |
| `bot auth status` | 저장된 자격 증명 목록 출력(`kind`, `state`, `version`, 만료, 엔드포인트 `host:port`): 선택 `--profile <p>`, `--json`. |
| `bot auth refresh` | 저장된 OAuth 또는 서비스 계정 자격 증명을 즉시 강제 갱신: `--profile <p> --name <n>`. |
| `bot auth logout` | PostgreSQL에서 관리형 자격 증명을 삭제하고 감사 로그 기록: `--profile <p> --name <n>`. |

DB 연결 점검과 함께 저장된 자격 증명 상태를 확인하려면 `bot doctor --check-db`를 실행하세요.
