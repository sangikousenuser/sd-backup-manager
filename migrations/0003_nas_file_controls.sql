-- CANCELLED 等の新しい永続状態を扱えない旧アプリが DB を開かないよう版を進める。
CREATE INDEX idx_transfers_session_state ON transfers(session_id, state);
