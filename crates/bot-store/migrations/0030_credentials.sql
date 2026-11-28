-- Host-encrypted OAuth, service-account, and opt-in subscription credentials with CAS refresh state.
create table bot_credentials (
    profile             text   not null,
    name                text   not null,
    kind                text   not null check (kind in ('oauth', 'service_account', 'subscription_token')),
    token_endpoint_host text   not null default '',
    nonce               bytea  not null,
    ciphertext          bytea  not null,
    version             bigint not null default 1 check (version >= 1),
    state               text   not null default 'active' check (state in ('active', 'refreshing', 'needs_login')),
    refresh_nonce       text,
    refresh_until_ms    bigint,
    expires_ms          bigint,
    updated_ms          bigint not null default bot_now_ms(),
    primary key (profile, name)
);
