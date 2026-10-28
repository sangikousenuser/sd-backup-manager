use super::*;
use backup_core::classifier::MediaKind;
use backup_core::identity::{Confidence, MediaDescriptor, PartitionDescriptor};
use backup_core::state::{NasFileState, SessionState, StageState};
use chrono::TimeZone;

fn media(uuid: &str) -> MediaDescriptor {
    MediaDescriptor {
        device_key: "/dev/sdx".into(),
        vendor: Some("Vendor".into()),
        model: Some("Card".into()),
        serial: Some("123".into()),
        bus: Some("usb".into()),
        size_bytes: 1000,
        partitions: vec![PartitionDescriptor {
            index: 1,
            device_path: "/dev/sdx1".into(),
            backend_id: "x".into(),
            fs_type: Some("exfat".into()),
            fs_uuid: Some(uuid.into()),
            part_guid: None,
            fs_label: None,
            size_bytes: 1000,
            mount_point: None,
        }],
    }
}

fn setup() -> (Db, String, String, SessionRow) {
    let db = Db::open_in_memory().unwrap();
    let m = media("AAAA-BBBB");
    let dev = db.upsert_device(&m, Confidence::High).unwrap();
    let parts = db.upsert_partitions(&dev.device_id, &m.partitions).unwrap();
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 10, 15, 0, 32, 15)
        .unwrap();
    let s = db
        .create_session(&dev.device_id, Confidence::High, now, "Asia/Tokyo")
        .unwrap();
    (db, dev.device_id, parts[0].partition_id.clone(), s)
}

fn new_file(session: &str, partition: &str, path: &str) -> NewFile {
    NewFile {
        session_id: session.into(),
        partition_id: partition.into(),
        partition_index: 1,
        source_rel_path: path.into(),
        source_rel_path_raw_hex: None,
        destination_rel_path: format!("Photos/P01/{path}"),
        media_kind: MediaKind::Photo,
        group_key: None,
        size_bytes: 10,
        source_mtime_ns: Some(1),
    }
}

#[test]
fn migrations_apply_and_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    {
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 2);
    assert_eq!(db.quick_check().unwrap(), "ok");
}

#[test]
fn folder_names_are_unique_within_same_second() {
    let (db, device_id, _, s1) = setup();
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 10, 15, 0, 32, 15)
        .unwrap();
    let s2 = db
        .create_session(&device_id, Confidence::High, now, "Asia/Tokyo")
        .unwrap();
    assert_eq!(s1.folder_name, "2026101509321501");
    assert_eq!(s2.folder_name, "2026101509321502");
    assert_ne!(s1.session_id, s2.session_id);
}

#[test]
fn card_numbers_survive_native_reformat_but_usb_readers_are_not_card_ids() {
    let db = Db::open_in_memory().unwrap();
    let mut before = media("before-format");
    let mut after = media("after-format");
    assert!(
        before.stable_card_key().is_none(),
        "USB リーダーのシリアルを使わない"
    );
    let a = db.upsert_device(&before, Confidence::High).unwrap();
    let first = db.assign_card(&a.device_id, None, None).unwrap();
    assert_eq!(first, "sd1");
    let b = db.upsert_device(&after, Confidence::High).unwrap();
    assert_ne!(a.device_id, b.device_id);
    assert!(db.known_card(&b.device_id, None).unwrap().is_none());
    assert_eq!(db.assign_card(&b.device_id, None, None).unwrap(), "sd2");

    before.bus = Some("sd".into());
    after.bus = Some("sd".into());
    let key = before.stable_card_key().unwrap();
    assert_eq!(after.stable_card_key().as_deref(), Some(key.as_str()));
    // 別DBでネイティブSD接続を模擬。
    let db = Db::open_in_memory().unwrap();
    let a = db.upsert_device(&before, Confidence::High).unwrap();
    let first = db.assign_card(&a.device_id, None, Some(&key)).unwrap();
    let b = db.upsert_device(&after, Confidence::High).unwrap();
    assert_eq!(
        db.known_card(&b.device_id, Some(&key)).unwrap(),
        Some(first)
    );
    assert!(db.find_resumable_session(&b.device_id).unwrap().is_none());
}

#[test]
fn card_sessions_use_fourteen_digits_and_resolve_same_second_collisions() {
    let (db, device_id, _, legacy) = setup();
    let card = db.assign_card(&device_id, None, None).unwrap();
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 10, 28, 2, 28, 34)
        .unwrap();
    let first = db
        .create_card_session(&device_id, &card, Confidence::High, now, "Asia/Tokyo")
        .unwrap();
    let second = db
        .create_card_session(&device_id, &card, Confidence::High, now, "Asia/Tokyo")
        .unwrap();
    assert_eq!(first.folder_name, "sd1/20261028112834");
    assert_eq!(second.folder_name, "sd1/2026102811283401");
    assert_eq!(
        db.get_session(&legacy.session_id)
            .unwrap()
            .unwrap()
            .folder_name,
        legacy.folder_name
    );
    assert_eq!(db.card_names().unwrap(), vec!["sd1"]);
}

#[test]
fn transitions_are_validated() {
    let (db, _, _, s) = setup();
    db.transition_session(&s.session_id, SessionState::Scanning)
        .unwrap();
    let err = db
        .transition_session(&s.session_id, SessionState::Complete)
        .unwrap_err();
    assert!(matches!(err, DbError::InvalidTransition { .. }));
    db.transition_session(&s.session_id, SessionState::Copying)
        .unwrap();
    db.transition_session(&s.session_id, SessionState::SourceInterrupted)
        .unwrap();
}

#[test]
fn identity_lookup_reuses_device_and_resumes_session() {
    let (db, device_id, _, s) = setup();
    let again = db
        .upsert_device(&media("AAAA-BBBB"), Confidence::High)
        .unwrap();
    assert_eq!(again.device_id, device_id);
    let other = db
        .upsert_device(&media("CCCC-DDDD"), Confidence::High)
        .unwrap();
    assert_ne!(other.device_id, device_id);

    assert!(db.find_resumable_session(&device_id).unwrap().is_none());
    db.transition_session(&s.session_id, SessionState::Scanning)
        .unwrap();
    db.transition_session(&s.session_id, SessionState::SourceInterrupted)
        .unwrap();
    let r = db.find_resumable_session(&device_id).unwrap().unwrap();
    assert_eq!(r.session_id, s.session_id);
    assert!(
        db.find_resumable_session(&other.device_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn file_lifecycle_and_transfer_queue_and_catalog() {
    let (db, _, part, s) = setup();
    let f = db
        .insert_file(&new_file(&s.session_id, &part, "DCIM/A.JPG"))
        .unwrap();
    assert_eq!(f.stage_state, StageState::Discovered);
    assert_eq!(db.begin_copy_attempt(&f.file_id).unwrap(), 1);
    assert_eq!(db.begin_copy_attempt(&f.file_id).unwrap(), 2);

    let hash = "a".repeat(64);
    db.mark_file_ssd_verified(&f.file_id, &hash).unwrap();
    let now = now_utc();
    let job = db.next_due_transfer(&now).unwrap().unwrap();
    assert_eq!(job.file_id, f.file_id);
    assert_eq!(db.pending_transfer_count(&s.session_id).unwrap(), 1);

    db.transfer_started(&job, ".tmp").unwrap();
    assert!(
        db.next_due_transfer(&now).unwrap().is_none(),
        "uploading jobs are not re-picked"
    );
    let future = to_db_time(chrono::Utc::now() + chrono::Duration::hours(1));
    db.transfer_retry(&job, "offline", &future).unwrap();
    assert!(
        db.next_due_transfer(&now).unwrap().is_none(),
        "retry waits for backoff"
    );
    assert_eq!(db.retry_all_transfers_now().unwrap(), 1);
    let job = db.next_due_transfer(&now_utc()).unwrap().unwrap();
    db.transfer_verified(&job, "smb://nas/share").unwrap();

    let f = db.get_file(&f.file_id).unwrap().unwrap();
    assert_eq!(f.nas_state, NasFileState::NasVerified);
    assert_eq!(db.pending_transfer_count(&s.session_id).unwrap(), 0);

    let identity = media("AAAA-BBBB").identity_hash();
    let fp = media("AAAA-BBBB").partitions[0].fingerprint();
    let c = db
        .catalog_candidates(&identity, &fp, "DCIM/A.JPG", 10)
        .unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].blake3_hash, hash);
    assert_eq!(c[0].nas_session_folder, s.folder_name);
    assert!(
        db.catalog_candidates(&identity, &fp, "DCIM/A.JPG", 11)
            .unwrap()
            .is_empty()
    );

    let s = db.refresh_session_counters(&s.session_id).unwrap();
    assert_eq!(s.total_files, 1);
    assert_eq!(s.completed_files, 1);
    assert_eq!(s.nas_verified_files, 1);
}

#[test]
fn unique_destination_paths_enforced() {
    let (db, _, part, s) = setup();
    db.insert_file(&new_file(&s.session_id, &part, "A.JPG"))
        .unwrap();
    let mut dup = new_file(&s.session_id, &part, "B.JPG");
    dup.destination_rel_path = "Photos/P01/A.JPG".into();
    assert!(db.insert_file(&dup).is_err());
}

#[test]
fn recovery_resets_in_flight_work() {
    let (db, _, part, s) = setup();
    db.transition_session(&s.session_id, SessionState::Scanning)
        .unwrap();
    db.transition_session(&s.session_id, SessionState::Copying)
        .unwrap();
    let f1 = db
        .insert_file(&new_file(&s.session_id, &part, "A.JPG"))
        .unwrap();
    let f2 = db
        .insert_file(&new_file(&s.session_id, &part, "B.JPG"))
        .unwrap();
    db.begin_copy_attempt(&f1.file_id).unwrap();
    db.mark_file_ssd_verified(&f2.file_id, &"b".repeat(64))
        .unwrap();
    let job = db.next_due_transfer(&now_utc()).unwrap().unwrap();
    db.transfer_started(&job, ".tmp").unwrap();

    let report = db.recover_after_restart().unwrap();
    assert_eq!(report.sessions_interrupted, vec![s.session_id.clone()]);
    assert_eq!(report.files_reset, 1);
    assert_eq!(report.transfers_reset, 1);
    let s = db.get_session(&s.session_id).unwrap().unwrap();
    assert_eq!(s.state, SessionState::SourceInterrupted);
    assert_eq!(
        db.get_file(&f1.file_id).unwrap().unwrap().stage_state,
        StageState::Discovered
    );
    assert_eq!(
        db.get_file(&f2.file_id).unwrap().unwrap().nas_state,
        NasFileState::Queued
    );
    assert!(db.next_due_transfer(&now_utc()).unwrap().is_some());
}

#[test]
fn events_purge_keeps_catalog() {
    let (db, _, _, s) = setup();
    db.log_event(Some(&s.session_id), "info", "test", "hello")
        .unwrap();
    assert_eq!(
        db.recent_events("2000-01-01T00:00:00.000Z", 10)
            .unwrap()
            .len(),
        1
    );
    let future = to_db_time(chrono::Utc::now() + chrono::Duration::days(1));
    assert_eq!(db.purge_events_before(&future).unwrap(), 1);
    assert_eq!(
        db.recent_events("2000-01-01T00:00:00.000Z", 10)
            .unwrap()
            .len(),
        0
    );
    assert!(db.get_session(&s.session_id).unwrap().is_some());
}

#[test]
fn reset_file_removes_transfer() {
    let (db, _, part, s) = setup();
    let f = db
        .insert_file(&new_file(&s.session_id, &part, "A.JPG"))
        .unwrap();
    db.mark_file_ssd_verified(&f.file_id, &"c".repeat(64))
        .unwrap();
    db.reset_file_to_discovered(&f.file_id, "missing on SSD")
        .unwrap();
    assert!(db.next_due_transfer(&now_utc()).unwrap().is_none());
    assert_eq!(db.pending_transfer_count(&s.session_id).unwrap(), 0);
    let f = db.get_file(&f.file_id).unwrap().unwrap();
    assert_eq!(f.stage_state, StageState::Discovered);
}
