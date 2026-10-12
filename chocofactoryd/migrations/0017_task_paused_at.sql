-- #295: when the task was asked to pause; set only while status is 'pausing' or 'paused'.
ALTER TABLE tasks ADD COLUMN paused_at TEXT;
