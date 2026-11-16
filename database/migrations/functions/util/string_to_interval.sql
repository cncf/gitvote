-- Converts a string to an interval, returning null when it is not a valid interval.
create or replace function string_to_interval(s text)
returns interval as $$
begin
    -- Convert the string into an interval
    return s::interval;
exception
    -- Report any string that cannot be converted as a missing interval
    when others then
        return null;
end;
$$ language plpgsql strict;
