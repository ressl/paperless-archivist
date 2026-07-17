-- One normalized login namespace for local, OIDC, and Paperless identities.
--
-- Normalization is deliberately owned by PostgreSQL so every writer and every
-- lookup uses the same collation/runtime: trim surrounding whitespace, lower
-- case, and treat an empty result as no identity. Usernames must have a claim;
-- a non-empty email has a claim in the same namespace. A user's own equal
-- username/email coalesce into one row with both flags.

create function normalize_user_identity(value text)
returns text
language sql
immutable
parallel safe
strict
as $$
  select nullif(lower(btrim(value)), '')
$$;

-- A rolling deployment may still have an old API process capable of writing
-- case-sensitive-only identities. Hold ACCESS EXCLUSIVE through preflight,
-- backfill, and trigger installation so no writer can commit a collision in
-- the gap after the diagnostic scan.
lock table users in access exclusive mode;

-- An empty email has always meant "not supplied" in the application. Canonical
-- NULLs keep that meaning while allowing the future non-empty constraint.
update users
   set email = null,
       updated_at = now()
 where email is not null
   and normalize_user_identity(email) is null;

-- Fail before installing any uniqueness structure. The complete migration is
-- transactional, so neither this normalization nor any schema object survives
-- a collision. Operators get every owner/kind/raw value and must rename the
-- identities explicitly; this migration never picks or merges an account.
do $$
declare
  blank_usernames text;
  collision_detail text;
begin
  select string_agg(format('user_id=%s username=%L', id, username), E'\n' order by id)
    into blank_usernames
    from users
   where normalize_user_identity(username) is null;

  if blank_usernames is not null then
    raise exception using
      message = 'blank normalized usernames prevent user identity migration',
      detail = blank_usernames,
      hint = 'Assign every listed account a non-empty username, then rerun the migration.';
  end if;

  with identity_values as (
    select id as user_id,
           'username'::text as identity_kind,
           username as raw_value,
           normalize_user_identity(username) as normalized_identity
      from users
    union all
    select id,
           'email'::text,
           email,
           normalize_user_identity(email)
      from users
     where normalize_user_identity(email) is not null
  ), collisions as (
    select normalized_identity,
           jsonb_agg(
             jsonb_build_object(
               'user_id', user_id,
               'kind', identity_kind,
               'value', raw_value
             )
             order by user_id, identity_kind
           ) as claims
      from identity_values
     group by normalized_identity
    having count(distinct user_id) > 1
  )
  select string_agg(
           format('normalized_identity=%L claims=%s', normalized_identity, claims),
           E'\n'
           order by normalized_identity
         )
    into collision_detail
    from collisions;

  if collision_detail is not null then
    raise exception using
      message = 'user identity normalization collision; migration aborted without merging accounts',
      detail = collision_detail,
      hint = 'Rename the listed usernames or emails so every normalized identity has one owner, then rerun the migration.';
  end if;
end $$;

alter table users
  add constraint users_username_normalized_nonempty_check
  check (normalize_user_identity(username) is not null);

alter table users
  add constraint users_email_normalized_nonempty_check
  check (email is null or normalize_user_identity(email) is not null);

create table users_identity_namespace (
  normalized_identity text primary key,
  user_id uuid not null references users(id) on delete cascade,
  username_claim boolean not null default false,
  email_claim boolean not null default false,
  constraint users_identity_namespace_has_kind_check
    check (username_claim or email_claim),
  constraint users_identity_namespace_is_normalized_check
    check (normalized_identity = normalize_user_identity(normalized_identity))
);

create unique index users_identity_namespace_one_username_idx
  on users_identity_namespace (user_id)
  where username_claim;

create unique index users_identity_namespace_one_email_idx
  on users_identity_namespace (user_id)
  where email_claim;

with identity_values as (
  select id as user_id,
         normalize_user_identity(username) as normalized_identity,
         true as username_claim,
         false as email_claim
    from users
  union all
  select id,
         normalize_user_identity(email),
         false,
         true
    from users
   where normalize_user_identity(email) is not null
), coalesced as (
  select normalized_identity,
         user_id,
         bool_or(username_claim) as username_claim,
         bool_or(email_claim) as email_claim
    from identity_values
   group by normalized_identity, user_id
)
insert into users_identity_namespace (
  normalized_identity, user_id, username_claim, email_claim
)
select normalized_identity, user_id, username_claim, email_claim
  from coalesced;

create function sync_users_identity_namespace()
returns trigger
language plpgsql
as $$
declare
  normalized_username text;
  normalized_email text;
begin
  normalized_username := normalize_user_identity(new.username);
  normalized_email := normalize_user_identity(new.email);

  delete from users_identity_namespace where user_id = new.id;

  insert into users_identity_namespace (
    normalized_identity, user_id, username_claim, email_claim
  )
  values (
    normalized_username,
    new.id,
    true,
    normalized_email is not null and normalized_email = normalized_username
  );

  if normalized_email is not null and normalized_email <> normalized_username then
    insert into users_identity_namespace (
      normalized_identity, user_id, username_claim, email_claim
    )
    values (normalized_email, new.id, false, true);
  end if;

  return new;
end $$;

create trigger users_identity_namespace_sync
after insert or update of username, email on users
for each row execute function sync_users_identity_namespace();
