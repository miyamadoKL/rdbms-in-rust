CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
INSERT INTO users (id) VALUES (1);
INSERT INTO users VALUES (2, 'Alice');
SELECT id FROM users WHERE name = 'Alice';
