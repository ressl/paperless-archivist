alter table users
  add column locked_until timestamptz;

create index users_locked_until_idx
  on users (locked_until)
  where locked_until is not null;
