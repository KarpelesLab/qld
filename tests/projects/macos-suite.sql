-- SQL run by the SQLite shell of tests/projects/macos-suite.sh, once built
-- with qld and once with Apple's linker; the two outputs must match.
.headers on
.mode list
PRAGMA journal_mode = WAL;
CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, value REAL, tags TEXT);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000)
INSERT INTO t(name, value, tags)
  SELECT printf('row%05d', i), (i * 7919) % 1000 / 10.0,
         json_array(i % 3, i % 5, 'x' || (i % 7))
  FROM n;
CREATE INDEX t_value ON t(value);
CREATE TRIGGER t_audit AFTER UPDATE ON t BEGIN
  INSERT INTO audit VALUES (old.id, old.value, new.value);
END;
CREATE TABLE audit(id, before, after);
UPDATE t SET value = value + 1 WHERE id % 1000 = 0;
SELECT count(*), sum(value), min(name), max(name) FROM t;
SELECT count(*), sum(after - before) FROM audit;
SELECT name, value, rank() OVER (ORDER BY value DESC, id) AS r FROM t
  ORDER BY r LIMIT 5;
SELECT tags ->> 2 AS tag, count(*) FROM t GROUP BY tag ORDER BY tag;
SELECT json_group_array(id) FROM (SELECT id FROM t WHERE value = 50.0 LIMIT 10);
SELECT round(sqrt(2), 6), round(exp(1), 6), round(ln(10), 6), pow(2, 20);
SELECT upper('mixed Case'), length(zeroblob(1000)), hex(zeroblob(2)), quote(x'00ff');
SELECT printf('%.3e|%10s|%-5d|%x', 12345.678, 'pad', 42, 255);
CREATE VIRTUAL TABLE docs USING fts5(body);
INSERT INTO docs(body) VALUES
  ('the quick brown fox'), ('jumps over the lazy dog'), ('a quick linker'),
  ('mach-o and elf'), ('the brown linker jumps');
SELECT rowid, highlight(docs, 0, '[', ']') FROM docs WHERE docs MATCH 'quick OR linker'
  ORDER BY rank, rowid;
CREATE VIRTUAL TABLE boxes USING rtree(id, x0, x1, y0, y1);
WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 99)
INSERT INTO boxes SELECT i, i, i + 5, i % 10, i % 10 + 2 FROM n;
SELECT count(*) FROM boxes WHERE x0 <= 50 AND x1 >= 40 AND y0 <= 3 AND y1 >= 1;
SELECT group_concat(name, ',') FROM (SELECT name FROM t ORDER BY name COLLATE NOCASE DESC LIMIT 3);
SELECT total_changes() > 0;
PRAGMA integrity_check;
