CREATE OR REPLACE FUNCTION user_count(target_org UUID)
RETURNS INTEGER
LANGUAGE plpgsql
AS $$
DECLARE
    total INTEGER;
BEGIN
    SELECT count(*) INTO total FROM users WHERE org_id = target_org;
    RETURN total;
END;
$$;
