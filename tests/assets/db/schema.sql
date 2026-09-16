CREATE SCHEMA IF NOT EXISTS "cdviz";

-- cdevents_lake
CREATE TABLE IF NOT EXISTS cdviz.cdevents_lake (
  "id" BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  "imported_at" TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "payload" JSONB NOT NULL
);

-- store_cdevent
create or replace procedure cdviz.store_cdevent(
    cdevent jsonb
)
as $$
begin
    insert into cdviz.cdevents_lake("payload") values(cdevent);
end;
$$ language plpgsql;

-- store_cdevents: batch variant, one round trip for many events
create or replace procedure cdviz.store_cdevents(
    cdevents jsonb[]
)
as $$
declare
    cdevent jsonb;
begin
    foreach cdevent in array cdevents
    loop
        begin
            call cdviz.store_cdevent(cdevent);
        exception when unique_violation then
            null;
        end;
    end loop;
end;
$$ language plpgsql;
