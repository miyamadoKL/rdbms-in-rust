CREATE TABLE orders (dept TEXT, amount BIGINT);
SELECT dept FROM orders WHERE COUNT(*) > 1;
