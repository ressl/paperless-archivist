alter table users
  add column external_auth_provider text,
  add column external_subject text;

create unique index users_external_identity_idx
  on users (external_auth_provider, external_subject)
  where external_auth_provider is not null and external_subject is not null;

create table oidc_login_states (
  state_hash text primary key,
  nonce text not null,
  pkce_verifier text not null,
  return_to text,
  expires_at timestamptz not null,
  created_at timestamptz not null default now()
);

create index oidc_login_states_expires_idx
  on oidc_login_states (expires_at);
