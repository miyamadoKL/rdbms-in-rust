CREATE TABLE orders (dept TEXT, amount BIGINT);
SELECT dept, amount FROM orders GROUP BY dept;
