CREATE TABLE orders (dept TEXT, amount BIGINT);
INSERT INTO orders VALUES ('eng', 100), ('eng', 200), ('sales', 50), ('sales', NULL), ('hr', NULL);
SELECT dept, COUNT(*), SUM(amount), MIN(amount), MAX(amount) FROM orders GROUP BY dept ORDER BY dept;
SELECT dept, COUNT(*) FROM orders GROUP BY dept HAVING COUNT(*) > 1 ORDER BY dept;
SELECT COUNT(*) FROM orders WHERE dept = 'nope';
