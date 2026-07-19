CREATE TABLE customers (id BIGINT NOT NULL, name TEXT);
CREATE TABLE orders (id BIGINT, customer_id BIGINT, item TEXT);
INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol');
INSERT INTO orders VALUES (10, 1, 'apple'), (11, 1, 'banana'), (12, 2, 'cherry'), (13, NULL, 'orphan'), (14, 99, 'nomatch');
SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id ORDER BY customers.name, orders.item;
EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id;
EXPLAIN SELECT customers.name FROM customers JOIN orders ON customers.id <> orders.customer_id;
SELECT id FROM customers JOIN orders ON customers.id = orders.customer_id;
SELECT customers.name FROM customers JOIN orders ON customers.id;
