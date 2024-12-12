-- cdevents_lake
CREATE TABLE IF NOT EXISTS "cdevents_lake" (
  "id" BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  "imported_at" TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "payload" JSONB NOT NULL
);

-- store_cdevent
create or replace procedure store_cdevent(
    cdevent jsonb
)
as $$
begin
    insert into "cdevents_lake"("payload") values(cdevent);
end;
$$ language plpgsql;
