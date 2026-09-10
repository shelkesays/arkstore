-- Fidelity fixture for the live PostgreSQL tests. Idempotent: drops what it creates.
DROP SCHEMA IF EXISTS shop CASCADE;
DROP TABLE IF EXISTS public.plain, public.pg_prefixed_ignore, public.skip_me, public.tsv;
DROP EXTENSION IF EXISTS btree_gist;
DROP EXTENSION IF EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE SCHEMA shop;
COMMENT ON SCHEMA shop IS 'shop objects';
CREATE TYPE shop.mood AS ENUM ('sad', 'ok', 'it''s great');
CREATE TYPE shop.money_pair AS (amount numeric(12,2), currency text);
CREATE DOMAIN shop.positive_int AS integer NOT NULL DEFAULT 1 CONSTRAINT positive_int_check CHECK (VALUE > 0);
CREATE TYPE shop.price_range AS RANGE (SUBTYPE = numeric);
CREATE SEQUENCE shop.order_number_seq START WITH 1000 INCREMENT BY 5 MINVALUE 1000 MAXVALUE 999999 CACHE 10 CYCLE;
SELECT nextval('shop.order_number_seq');
CREATE TABLE shop.customers (
  id serial PRIMARY KEY,
  name text NOT NULL,
  email text UNIQUE,
  mood shop.mood DEFAULT 'ok',
  note text COLLATE "C",
  created_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT name_not_blank CHECK (length(name) > 0)
);
COMMENT ON TABLE shop.customers IS 'people who buy';
COMMENT ON COLUMN shop.customers.email IS 'unique contact';
CREATE TABLE shop.orders (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  customer_id integer NOT NULL REFERENCES shop.customers(id) ON DELETE CASCADE,
  parent_order bigint REFERENCES shop.orders(id),
  total numeric(10,2) NOT NULL DEFAULT 0,
  total_cents bigint GENERATED ALWAYS AS ((total * 100)::bigint) STORED,
  qty shop.positive_int,
  pair shop.money_pair,
  payload jsonb,
  raw bytea,
  placed_at timestamptz DEFAULT now()
);
CREATE INDEX orders_customer_idx ON shop.orders (customer_id) WHERE total > 10;
CREATE INDEX orders_lower_idx ON shop.orders (lower(payload::text));
CREATE UNLOGGED TABLE shop.scratch (k text PRIMARY KEY, v int);
CREATE TABLE shop."Mixed Case" (id int PRIMARY KEY, "Weird Col" text);
CREATE TABLE shop.events (id bigserial, at date NOT NULL, kind text) PARTITION BY RANGE (at);
CREATE TABLE shop.events_2025 PARTITION OF shop.events FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
CREATE TABLE shop.events_default PARTITION OF shop.events DEFAULT;
CREATE INDEX events_kind_idx ON shop.events (kind);
ALTER TABLE shop.events ADD PRIMARY KEY (id, at);
CREATE TABLE shop.base_log (id int, msg text);
CREATE TABLE shop.child_log (extra text) INHERITS (shop.base_log);
CREATE TABLE shop.a_cycle (id int PRIMARY KEY, b_id int);
CREATE TABLE shop.b_cycle (id int PRIMARY KEY, a_id int REFERENCES shop.a_cycle(id));
ALTER TABLE shop.a_cycle ADD FOREIGN KEY (b_id) REFERENCES shop.b_cycle(id) DEFERRABLE INITIALLY DEFERRED;
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE TABLE shop.exclusive (room int, during tsrange, EXCLUDE USING gist (room WITH =, during WITH &&));
CREATE FUNCTION shop.total_for(cust integer) RETURNS numeric LANGUAGE sql STABLE AS $$ SELECT coalesce(sum(total), 0) FROM shop.orders WHERE customer_id = cust $$;
CREATE FUNCTION shop.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.placed_at := now(); RETURN NEW; END $$;
CREATE PROCEDURE shop.noop() LANGUAGE sql AS $$ SELECT 1 $$;
CREATE TRIGGER orders_touch BEFORE INSERT ON shop.orders FOR EACH ROW EXECUTE FUNCTION shop.touch();
CREATE TRIGGER orders_touch_disabled BEFORE UPDATE ON shop.orders FOR EACH ROW EXECUTE FUNCTION shop.touch();
ALTER TABLE shop.orders DISABLE TRIGGER orders_touch_disabled;
CREATE VIEW shop.customer_totals AS SELECT c.id, c.name, shop.total_for(c.id) AS total FROM shop.customers c;
CREATE MATERIALIZED VIEW shop.mood_counts AS SELECT mood, count(*) AS n FROM shop.customers GROUP BY mood;
CREATE UNIQUE INDEX mood_counts_mood_idx ON shop.mood_counts (mood);
ALTER TABLE shop.customers ENABLE ROW LEVEL SECURITY;
CREATE POLICY see_all ON shop.customers FOR SELECT USING (true);
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'reader') THEN CREATE ROLE reader NOLOGIN; END IF; END $$;
GRANT SELECT ON shop.customers TO reader;
GRANT USAGE ON SCHEMA shop TO reader;
CREATE TABLE public.plain (id int PRIMARY KEY, txt text, f double precision, t time, i interval, b boolean, arr int[]);
CREATE TABLE public.pg_prefixed_ignore (id int);
CREATE TABLE public.skip_me (id int);
CREATE TABLE public.tsv (doc tsvector);
CREATE INDEX tsv_trgm ON public.plain USING gin (txt gin_trgm_ops);
INSERT INTO shop.customers (name, email, mood, note) VALUES ('Ann', 'ann@example.com', 'sad', E'tab\there'), ('Bob', NULL, 'it''s great', E'new\nline'), ('Cé', 'ce@example.com', DEFAULT, '\N literal');
INSERT INTO shop.orders (customer_id, total, qty, pair, payload, raw) VALUES (1, 12.50, 2, ROW(12.50,'EUR'), '{"a":1}', '\xdeadbeef'), (2, 0, 1, NULL, NULL, NULL);
INSERT INTO shop.orders (customer_id, parent_order, total) VALUES (1, 1, 99.99);
INSERT INTO shop.events (at, kind) VALUES ('2025-06-01', 'x'), ('2024-01-01', 'old');
INSERT INTO shop.child_log VALUES (1, 'm', 'e');
BEGIN;
INSERT INTO shop.a_cycle VALUES (1, 1);
INSERT INTO shop.b_cycle VALUES (1, 1);
COMMIT;
INSERT INTO shop.scratch VALUES ('k', 1);
INSERT INTO shop."Mixed Case" VALUES (1, 'x');
INSERT INTO public.plain VALUES (1, 'hello', 1.5, '12:34:56', '1 day 2 hours', true, '{1,2}'), (2, NULL, 'NaN', NULL, NULL, NULL, NULL);
INSERT INTO public.skip_me VALUES (1);
INSERT INTO public.pg_prefixed_ignore VALUES (1);
REFRESH MATERIALIZED VIEW shop.mood_counts;
