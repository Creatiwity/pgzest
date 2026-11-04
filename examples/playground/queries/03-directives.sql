-- Directives, ESLint-style. Compare with 02-errors.sql.

-- Nothing is reported on the next line:
-- pgzest-disable-next-line
SELECT nope FROM users;

-- Only column-not-found is silenced; the other rules stay active:
-- pgzest-disable-next-line column-not-found -- column added by migration 42
SELECT also_missing FROM users;

-- A justification after a second `--` is ignored by the directive
-- parser, so document freely.

-- Block: everything is silenced between disable and enable.
-- pgzest-disable
SELECT missing_a FROM users;
SELECT missing_b FROM users;
-- pgzest-enable

-- Outside the block, checking resumes → this one should be underlined:
SELECT missing_c FROM users;
