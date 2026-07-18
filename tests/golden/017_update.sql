CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');
UPDATE users SET name = 'Carol' WHERE id = 1;
SELECT id, name FROM users WHERE id = 1;
