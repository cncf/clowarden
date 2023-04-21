create extension if not exists pgcrypto;

create table if not exists reconciliation (
    reconciliation_id uuid primary key default gen_random_uuid(),
    completed_at timestamptz default current_timestamp not null,
    error text,
    pr_number bigint,
    pr_created_by text,
    pr_merged_by text,
    pr_merged_at timestamptz
);

create table if not exists change (
    change_id uuid primary key default gen_random_uuid(),
    service text not null check (service <> ''),
    kind text not null check (kind <> ''),
    extra jsonb,
    applied_at timestamptz not null,
    error text,
    tsdoc tsvector not null,
    reconciliation_id uuid not null references reconciliation on delete restrict
);

create index change_reconciliation_id_idx on change (reconciliation_id);
