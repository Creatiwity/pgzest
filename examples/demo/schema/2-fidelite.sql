-- Points de fidélité, crédités quand une commande est servie.

CREATE FUNCTION award_loyalty() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    v_total  BIGINT;
    v_points INTEGER;
BEGIN
    IF NEW.status = 'served' AND OLD.status IS DISTINCT FROM 'served'
       AND NEW.customer_id IS NOT NULL THEN
        v_total := order_total(NEW.id);
        v_points := (v_total / 100)::integer;
        UPDATE customers
        SET loyalty_points = loyalty_points + v_points
        WHERE id = NEW.customer_id;
        RAISE NOTICE 'commande % : % point(s)', NEW.id, v_points;
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER orders_award_loyalty AFTER UPDATE OF status ON orders
    FOR EACH ROW EXECUTE FUNCTION award_loyalty();
