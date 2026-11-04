-- Loyalty points, credited when an order is served.
--
-- Written with macros, the routines record their runs at the `[log]` level
-- of pgweave.toml: with the `notice` target, src/main.rs traces them — the
-- trigger, then the function it calls, nested under the statement that
-- served the order.

-- One point per euro.
@function loyalty_points_of(p_order BIGINT): INTEGER {
    RETURN (order_total(p_order) / 100)::integer;
}

@trigger orders_award_loyalty after update of status on orders for each row
    when (NEW.status = 'served' AND OLD.status IS DISTINCT FROM 'served'
          AND NEW.customer_id IS NOT NULL) {
    UPDATE customers
    SET loyalty_points = loyalty_points + loyalty_points_of(NEW.id)
    WHERE id = NEW.customer_id;
    RETURN NULL;
}
