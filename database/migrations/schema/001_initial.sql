create extension if not exists pgcrypto;

create table if not exists vote (
    vote_id uuid primary key default gen_random_uuid(),
    vote_comment_id bigint not null,
    created_at timestamptz default current_timestamp not null,
    ends_at timestamptz not null,
    closed boolean not null default false,
    closed_at timestamptz,
    event jsonb not null,
    metadata jsonb not null,
    results jsonb
);
