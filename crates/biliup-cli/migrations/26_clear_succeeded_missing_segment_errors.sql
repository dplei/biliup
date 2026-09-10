UPDATE upload_missing_segment
SET last_error = NULL
WHERE status = 'succeeded' AND last_error IS NOT NULL;
