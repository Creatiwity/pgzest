CREATE EXTENSION IF NOT EXISTS citext;

CREATE DOMAIN email AS citext CHECK (VALUE ~ '@');
CREATE DOMAIN money_amount AS NUMERIC(14, 2) NOT NULL CHECK (VALUE >= 0);

CREATE TYPE address AS (street TEXT, city TEXT, zip VARCHAR(10));
CREATE TYPE plan AS ENUM ('free', 'pro', 'team');

CREATE SCHEMA billing;

CREATE TABLE accounts (
    id         SERIAL PRIMARY KEY,
    login      email NOT NULL,
    plan       plan NOT NULL DEFAULT 'free',
    plans_seen plan[] NOT NULL DEFAULT '{}',
    home       address,
    balance    money_amount DEFAULT 0,
    created    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE billing.invoices (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts (id),
    amount     money_amount,
    issued_on  DATE NOT NULL,
    due_on     DATE GENERATED ALWAYS AS (issued_on + 30) STORED,
    lines      JSONB NOT NULL DEFAULT '[]'
);

CREATE VIEW account_totals AS
SELECT a.id, a.login, a.plan, coalesce(sum(i.amount), 0) AS total, count(i.id) AS invoices,
       max(i.issued_on) AS last_issue
FROM accounts a LEFT JOIN billing.invoices i ON i.account_id = a.id
GROUP BY a.id;

CREATE VIEW billing.overdue AS
SELECT i.*, current_date - i.due_on AS days_late FROM billing.invoices i WHERE i.due_on < current_date;

CREATE FUNCTION invoices_of(p_account INTEGER)
RETURNS SETOF billing.invoices LANGUAGE sql STABLE AS $$
    SELECT * FROM billing.invoices WHERE account_id = p_account
$$;

CREATE FUNCTION plan_counts()
RETURNS TABLE (plan plan, n BIGINT) LANGUAGE sql STABLE AS $$
    SELECT plan, count(*) FROM accounts GROUP BY plan
$$;

CREATE FUNCTION account_label(a accounts) RETURNS TEXT LANGUAGE sql IMMUTABLE AS $$
    SELECT a.login || ' (' || a.plan || ')'
$$;

CREATE FUNCTION split_amount(total NUMERIC, parts INTEGER, OUT share NUMERIC, OUT remainder NUMERIC)
LANGUAGE sql IMMUTABLE AS $$
    SELECT trunc(total / parts, 2), total - trunc(total / parts, 2) * parts
$$;
