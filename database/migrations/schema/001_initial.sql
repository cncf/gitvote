create extension if not exists pgcrypto;

create table if not exists vote (
    vote_id uuid primary key default gen_random_uuid(),
    vote_comment_id bigint not null,
    created_at timestamptz default current_timestamp not null,
    created_by text not null,
    ends_at timestamptz not null,
    closed boolean not null default false,
    closed_at timestamptz,
    cfg jsonb not null,
    installation_id bigint not null,
    issue_id bigint not null,
    issue_number bigint not null,
    is_pull_request boolean not null,
    repository_full_name text not null,
    organization text,
    results jsonb
);
