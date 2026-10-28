-- 保存先のカード番号と、フォーマットに依存する再開用識別子を分離する。
CREATE TABLE cards (
  card_id INTEGER PRIMARY KEY AUTOINCREMENT,
  stable_key TEXT UNIQUE
);
CREATE TABLE device_cards (
  device_id TEXT PRIMARY KEY REFERENCES devices(device_id),
  card_id INTEGER NOT NULL REFERENCES cards(card_id)
);
CREATE TABLE session_directories (
  session_id TEXT NOT NULL REFERENCES sessions(session_id),
  partition_index INTEGER NOT NULL,
  source_rel_path TEXT NOT NULL,
  destination_rel_path TEXT NOT NULL,
  PRIMARY KEY(session_id, partition_index, source_rel_path)
);
