-- Add 'withdrawal' to billing_event_type so hospital wallet withdrawals to an
-- external bank account can be recorded in billing_transactions, alongside the
-- 'deposit' / 'payout' values added in 20240031. The label is only added here;
-- it is first *used* at runtime, so this is safe in sqlx's per-migration txn.
ALTER TYPE billing_event_type ADD VALUE IF NOT EXISTS 'withdrawal';
