CREATE TABLE pages (
    id    BIGINT PRIMARY KEY,
    title TEXT NOT NULL,
    views INTEGER NOT NULL DEFAULT 0
);

CREATE FUNCTION page_stats(min_views INTEGER)
RETURNS TABLE (page_id BIGINT, page_title TEXT, score DOUBLE PRECISION)
LANGUAGE sql STABLE AS $$
    SELECT id, title, views::double precision / 100 FROM pages WHERE views >= min_views
$$;

CREATE FUNCTION page_title(p_id BIGINT) RETURNS TEXT
LANGUAGE plpgsql STABLE AS $$
DECLARE
    result TEXT;
BEGIN
    SELECT title INTO result FROM pages WHERE id = p_id;
    RETURN result;
END;
$$;

CREATE PROCEDURE reset_views(p_id BIGINT)
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE pages SET views = 0 WHERE id = p_id;
END;
$$;
