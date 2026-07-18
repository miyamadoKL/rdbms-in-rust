CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');
SELECT id FROM users WHERE id > 1;
