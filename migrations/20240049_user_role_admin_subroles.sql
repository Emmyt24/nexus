-- Add the three admin sub-roles to the user_role enum (Admin §1.1).
-- Must live alone in this file: Postgres forbids using a new enum value in the
-- same transaction that adds it, and sqlx runs each migration file in one tx.
ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'operations_admin';
ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'verification_admin';
ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'finance_admin';
