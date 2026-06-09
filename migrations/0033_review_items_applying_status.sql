-- #253: add an intermediate 'applying' status to review_items so the apply
-- path can atomically claim a row before patching Paperless. Without it,
-- two concurrent applies (two operators, or an operator racing the autopilot
-- drain) both read an 'approved'/'edited' row, both PATCH Paperless, and both
-- record review.applied. 'applying' is the "owned, about to patch" state:
-- the claim is a conditional UPDATE, so only one actor wins.

alter table review_items
  drop constraint if exists review_items_status_check;

alter table review_items
  add constraint review_items_status_check
  check (status in ('pending', 'approved', 'rejected', 'edited', 'applying', 'applied'));
