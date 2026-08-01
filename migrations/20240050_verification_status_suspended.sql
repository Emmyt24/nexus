-- Add a 'suspended' state to verification_status so admins can suspend hospitals
-- independently of the pending -> approved registration flow (Admin §1.2).
ALTER TYPE verification_status ADD VALUE IF NOT EXISTS 'suspended';
