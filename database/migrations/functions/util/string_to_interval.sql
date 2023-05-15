-- Helper function to validate an interval.
-- Source: https://stackoverflow.com/a/36777132
create or replace function string_to_interval(s text)
returns interval as $$
begin
    return s::interval;
exception when others then
    return null;
end;
$$ language plpgsql strict;
