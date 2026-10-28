//! ユーザーが指定した PC コピーの削除と、中断後の再開。
use crate::state::Shared;
use backup_core::state::StageState;
use backup_db::FileRow;

pub fn finish_deletion(shared: &Shared, file: &FileRow) -> Result<(), String> {
    if file.stage_state != StageState::DeletePending {
        return Err("削除指定のないファイルです".into());
    }
    let result = (|| {
        shared
            .staging
            .remove_file(&file.session_id, &file.destination_rel_path)
            .map_err(|e| e.to_string())?;
        shared
            .staging
            .remove_file(&file.session_id, &format!(".partial/{}.part", file.file_id))
            .map_err(|e| e.to_string())?;
        shared
            .db()
            .finish_nas_file_deletion(&file.file_id)
            .map_err(|e| e.to_string())
    })();
    if let Err(error) = &result {
        let _ = shared.db().set_file_error(
            &file.file_id,
            StageState::DeletePending,
            "USER_DELETE_FAILED",
            error,
        );
    }
    result
}

pub fn refresh_manifest(shared: &Shared, sid: &str) -> anyhow::Result<()> {
    let session = shared
        .db()
        .get_session(sid)?
        .ok_or_else(|| anyhow::anyhow!("セッションがありません"))?;
    let files = shared.db().files_for_session(sid)?;
    let manifest = crate::nas_worker::build_manifest(shared, &session, &files)?;
    crate::nas_worker::write_atomic(
        &shared.staging.manifest_path(sid)?,
        &manifest.to_json_pretty(),
    )?;
    Ok(())
}
