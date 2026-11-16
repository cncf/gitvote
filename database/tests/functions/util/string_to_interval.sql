-- Tests converting a string to an interval.

-- ============================================================================
-- SETUP
-- ============================================================================

begin;
select plan(9);

-- ============================================================================
-- TESTS
-- ============================================================================

-- Should convert a negative interval
select is(
    string_to_interval('-1 day'),
    '-1 day'::interval,
    'Should convert a negative interval'
);

-- Should convert a number of days
select is(
    string_to_interval('5 days'),
    '5 days'::interval,
    'Should convert a number of days'
);

-- Should convert a number of hours
select is(
    string_to_interval('24 hours'),
    '24 hours'::interval,
    'Should convert a number of hours'
);

-- Should convert a number of weeks
select is(
    string_to_interval('1 week'),
    '7 days'::interval,
    'Should convert a number of weeks'
);

-- Should convert a plain number as seconds
select is(
    string_to_interval('5'),
    '5 seconds'::interval,
    'Should convert a plain number as seconds'
);

-- Should return null for a misspelled unit
select is(
    string_to_interval('5 dais'),
    null::interval,
    'Should return null for a misspelled unit'
);

-- Should return null for a null string
select is(
    string_to_interval(null),
    null::interval,
    'Should return null for a null string'
);

-- Should return null for a value out of range
select is(
    string_to_interval('178956971 years'),
    null::interval,
    'Should return null for a value out of range'
);

-- Should return null for an empty string
select is(
    string_to_interval(''),
    null::interval,
    'Should return null for an empty string'
);

-- ============================================================================
-- CLEANUP
-- ============================================================================

select * from finish();
rollback;
