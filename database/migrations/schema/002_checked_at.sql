alter table vote add column checked_at timestamptz;

---- create above / drop below ----

alter table vote drop column checked_at;
