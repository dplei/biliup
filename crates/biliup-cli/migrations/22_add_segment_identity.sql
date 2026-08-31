-- The stable segment identity is assigned when the recording file is created, so events emitted
-- before enrollment can already name the segment. Additive and nullable: older builds keep
-- reading and writing this table unchanged, and rows written before this migration stay valid.
alter table upload_missing_segment add column segment_id TEXT;

create index if not exists ix_upload_segment_identity
    on upload_missing_segment (segment_id)
    where segment_id is not null;
