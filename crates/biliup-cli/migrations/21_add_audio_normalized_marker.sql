-- The loudness-normalized output now replaces the original recording in place, so a recovery
-- upload reading `file_path` gets an already-normalized file. Without this marker it would
-- measure ~-16 LUFS, apply a ~0 dB gain, and still pay for a full AAC re-encode every retry.
--
-- NOTE: this is unrelated to `normalized_file_path`, which holds a *path canonicalization*
-- result used by the uniqueness indexes.
alter table upload_missing_segment add column audio_normalized_at DATETIME;
