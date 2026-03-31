-- Add spam filtering columns
ALTER TABLE emails ADD COLUMN spam_score FLOAT;
ALTER TABLE emails ADD COLUMN spam_verdict TEXT;

ALTER TABLE accounts ADD COLUMN spam_enabled BOOLEAN DEFAULT true;
ALTER TABLE accounts ADD COLUMN spam_threshold FLOAT DEFAULT 0.5;
