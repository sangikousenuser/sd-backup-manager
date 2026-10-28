use crate::ipc::{self, DaemonEvent};
use crate::views;
use backup_ipc::{
    Command, EventSummary, FileSummary, NasFileAction, NasTestReport, NotificationKind, Response,
    SessionSummary, SettingsView, Snapshot,
};
use iced::{Element, Subscription, Task, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Details(usize),
    History,
    Settings,
}

#[derive(Debug, Clone)]
pub enum Message {
    Daemon(DaemonEvent),
    Navigate(Screen),
    Run(Command),
    OpenFolder(String),
    FolderOpened(Result<(), String>),
    CommandDone(Result<Response, String>),
    // 履歴
    HistoryLoaded(Result<Response, String>, Result<Response, String>),
    SelectSession(String),
    FilesLoaded(String, u32, bool, Result<Response, String>),
    RefreshFiles,
    FilesPage(u32),
    OnlyProblemFiles(bool),
    ToggleFile(String, bool),
    SelectPageFiles(bool),
    RequestNasAction(NasFileAction),
    ConfirmNasAction,
    CancelNasAction,
    RestoreDest(String),
    RestoreSession(String),
    DiscardSession(String),
    ExportDir(String),
    ExportLogs,
    // 設定
    SettingsLoaded(Result<Response, String>),
    NasMode(String),
    NasUrl(String),
    NasUser(String),
    NasDomain(String),
    NasPassword(String),
    NasMountedPath(String),
    FinalRehash(bool),
    SuccessSound(bool),
    WarningSound(bool),
    TestNas,
    TestDone(Result<Response, String>),
    SaveSettings,
    SaveDone(Result<Response, String>),
    InitTarget,
    PlaySound(NotificationKind),
}

pub struct App {
    pub screen: Screen,
    pub snapshot: Option<Snapshot>,
    pub connection_error: Option<String>,
    pub last_seq: Option<u64>,
    pub toast: Option<(bool, String)>,
    pub busy: bool,
    // 履歴
    pub sessions: Vec<SessionSummary>,
    pub events: Vec<EventSummary>,
    pub selected_session: Option<String>,
    pub files: Vec<FileSummary>,
    pub selected_files: std::collections::HashSet<String>,
    pub files_offset: u32,
    pub files_total: u64,
    pub only_problem_files: bool,
    pub files_loading: bool,
    pub pending_nas_action: Option<(NasFileAction, Vec<String>)>,
    pub restore_dest: String,
    pub export_dir: String,
    // 設定
    pub settings: SettingsView,
    pub settings_loaded: bool,
    pub password: String,
    pub password_stored: bool,
    pub test_report: Option<NasTestReport>,
}

impl App {
    pub fn new() -> (Self, Task<Message>) {
        let home = backup_config::home_dir();
        let app = Self {
            screen: Screen::Main,
            snapshot: None,
            connection_error: None,
            last_seq: None,
            toast: None,
            busy: false,
            sessions: Vec::new(),
            events: Vec::new(),
            selected_session: None,
            files: Vec::new(),
            selected_files: Default::default(),
            files_offset: 0,
            files_total: 0,
            only_problem_files: true,
            files_loading: false,
            pending_nas_action: None,
            restore_dest: home.join("SDRestore").display().to_string(),
            export_dir: home.join("Desktop").display().to_string(),
            settings: SettingsView::default(),
            settings_loaded: false,
            password: String::new(),
            password_stored: false,
            test_report: None,
        };
        (
            app,
            Task::perform(ipc::send(Command::GetSettings), Message::SettingsLoaded),
        )
    }

    pub fn title(&self) -> String {
        "SD Backup Manager".into()
    }

    pub fn theme(&self) -> Theme {
        Theme::Light
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let daemon = Subscription::run(ipc::subscribe).map(Message::Daemon);
        if self.screen == Screen::History && self.selected_session.is_some() {
            Subscription::batch([
                daemon,
                iced::time::every(std::time::Duration::from_secs(3)).map(|_| Message::RefreshFiles),
            ])
        } else {
            daemon
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Daemon(DaemonEvent::Snapshot(snap)) => {
                self.connection_error = None;
                self.handle_notification(&snap);
                self.snapshot = Some(*snap);
                if !self.settings_loaded {
                    return Task::perform(ipc::send(Command::GetSettings), Message::SettingsLoaded);
                }
            }
            Message::Daemon(DaemonEvent::Disconnected(e)) => {
                self.connection_error = Some(e);
            }
            Message::Navigate(screen) => {
                self.pending_nas_action = None;
                self.screen = screen;
                self.toast = None;
                match screen {
                    Screen::History => return self.load_history(),
                    Screen::Settings => {
                        self.test_report = None;
                        return Task::perform(
                            ipc::send(Command::GetSettings),
                            Message::SettingsLoaded,
                        );
                    }
                    _ => {}
                }
            }
            Message::Run(command) => {
                self.busy = true;
                return Task::perform(ipc::send(command), Message::CommandDone);
            }
            Message::OpenFolder(path) => {
                return Task::perform(
                    async move { crate::platform::open_folder(&path) },
                    Message::FolderOpened,
                );
            }
            Message::FolderOpened(result) => {
                if let Err(e) = result {
                    self.toast = Some((false, e));
                }
            }
            Message::CommandDone(result) => {
                self.busy = false;
                self.set_toast(result);
                if self.screen == Screen::History {
                    return Task::batch([self.load_history(), self.load_files()]);
                }
            }
            Message::HistoryLoaded(sessions, events) => {
                if let Ok(Response::Sessions { sessions }) = sessions {
                    self.sessions = sessions;
                }
                if let Ok(Response::Events { events }) = events {
                    self.events = events;
                }
            }
            Message::SelectSession(id) => {
                self.selected_session = Some(id);
                self.files.clear();
                self.selected_files.clear();
                self.pending_nas_action = None;
                self.files_offset = 0;
                self.files_total = 0;
                return self.load_files();
            }
            Message::FilesLoaded(sid, offset, problems, r) => {
                if self.selected_session.as_deref() != Some(&sid)
                    || self.files_offset != offset
                    || self.only_problem_files != problems
                {
                    return Task::none();
                }
                self.files_loading = false;
                match r {
                    Ok(Response::Files { files, total_files }) => {
                        self.files_total = total_files;
                        self.files = files;
                        self.selected_files.retain(|id| {
                            self.files
                                .iter()
                                .any(|f| f.file_id == *id && file_is_actionable(f))
                        });
                        if self.files.is_empty() && offset > 0 {
                            self.files_offset = total_files.saturating_sub(1) as u32 / 200 * 200;
                            return self.load_files();
                        }
                    }
                    Err(e) => self.toast = Some((false, e)),
                    _ => {}
                }
            }
            Message::RefreshFiles => {
                if !self.busy && !self.files_loading && self.pending_nas_action.is_none() {
                    return self.load_files();
                }
            }
            Message::FilesPage(offset) => {
                if self.busy || self.pending_nas_action.is_some() {
                    return Task::none();
                }
                self.files_offset = offset;
                self.selected_files.clear();
                return self.load_files();
            }
            Message::OnlyProblemFiles(problems) => {
                self.only_problem_files = problems;
                self.files_offset = 0;
                self.selected_files.clear();
                self.pending_nas_action = None;
                return self.load_files();
            }
            Message::ToggleFile(id, selected) => {
                if !self.busy && self.pending_nas_action.is_none() {
                    if selected
                        && self
                            .files
                            .iter()
                            .any(|f| f.file_id == id && file_is_actionable(f))
                    {
                        self.selected_files.insert(id);
                    } else {
                        self.selected_files.remove(&id);
                    }
                }
            }
            Message::SelectPageFiles(select) => {
                if !self.busy && self.pending_nas_action.is_none() {
                    self.selected_files = if select {
                        self.files
                            .iter()
                            .filter(|f| file_is_actionable(f))
                            .map(|f| f.file_id.clone())
                            .collect()
                    } else {
                        Default::default()
                    };
                }
            }
            Message::RequestNasAction(action) => {
                if self.busy || self.files_loading || self.selected_files.is_empty() {
                    return Task::none();
                }
                let ids = self.selected_files.iter().cloned().collect();
                self.pending_nas_action = Some((action, ids));
                if action == NasFileAction::Retry {
                    return self.confirm_nas_action();
                }
            }
            Message::ConfirmNasAction => return self.confirm_nas_action(),
            Message::CancelNasAction => self.pending_nas_action = None,
            Message::RestoreDest(s) => self.restore_dest = s,
            Message::RestoreSession(id) => {
                self.busy = true;
                return Task::perform(
                    ipc::send(Command::RestoreLayout {
                        session_id: id,
                        dest_dir: self.restore_dest.clone(),
                    }),
                    Message::CommandDone,
                );
            }
            Message::DiscardSession(id) => {
                self.busy = true;
                return Task::perform(
                    ipc::send(Command::DiscardSession { session_id: id }),
                    Message::CommandDone,
                );
            }
            Message::ExportDir(s) => self.export_dir = s,
            Message::ExportLogs => {
                self.busy = true;
                return Task::perform(
                    ipc::send(Command::ExportLogs {
                        dest_dir: self.export_dir.clone(),
                    }),
                    Message::CommandDone,
                );
            }
            Message::SettingsLoaded(r) => {
                if let Ok(Response::Settings {
                    settings,
                    password_stored,
                }) = r
                {
                    self.settings = settings;
                    self.password_stored = password_stored;
                    self.settings_loaded = true;
                }
            }
            Message::NasMode(m) => self.settings.nas_mode = m,
            Message::NasUrl(s) => self.settings.nas_url = s,
            Message::NasUser(s) => self.settings.nas_username = s,
            Message::NasDomain(s) => self.settings.nas_domain = s,
            Message::NasPassword(s) => self.password = s,
            Message::NasMountedPath(s) => self.settings.nas_mounted_path = s,
            Message::FinalRehash(b) => self.settings.final_rehash_before_cleanup = b,
            Message::SuccessSound(b) => self.settings.play_success_sound = b,
            Message::WarningSound(b) => self.settings.play_warning_sound = b,
            Message::TestNas => {
                self.busy = true;
                self.test_report = None;
                return Task::perform(
                    ipc::send(Command::TestNas {
                        settings: self.settings.clone(),
                        password: self.password_opt(),
                    }),
                    Message::TestDone,
                );
            }
            Message::TestDone(r) => {
                self.busy = false;
                match r {
                    Ok(Response::NasTest { report }) => self.test_report = Some(report),
                    other => self.set_toast(other),
                }
            }
            Message::SaveSettings => {
                self.busy = true;
                return Task::perform(
                    ipc::send(Command::UpdateSettings {
                        settings: self.settings.clone(),
                        password: self.password_opt(),
                    }),
                    Message::SaveDone,
                );
            }
            Message::SaveDone(r) => {
                self.busy = false;
                let ok = r.is_ok();
                let needs_init =
                    matches!(&r, Ok(Response::Ok { message }) if message.contains("初期化"));
                self.set_toast(r);
                if ok {
                    self.password.clear();
                    let reload =
                        Task::perform(ipc::send(Command::GetSettings), Message::SettingsLoaded);
                    if needs_init {
                        return Task::batch([
                            Task::perform(
                                ipc::send(Command::InitializeNasTarget),
                                Message::CommandDone,
                            ),
                            reload,
                        ]);
                    }
                    return reload;
                }
            }
            Message::PlaySound(kind) => crate::sound::play(kind),
            Message::InitTarget => {
                self.busy = true;
                return Task::batch([
                    Task::perform(
                        ipc::send(Command::InitializeNasTarget),
                        Message::CommandDone,
                    ),
                    Task::perform(ipc::send(Command::GetSettings), Message::SettingsLoaded),
                ]);
            }
        }
        Task::none()
    }

    fn password_opt(&self) -> Option<String> {
        if self.password.is_empty() {
            None
        } else {
            Some(self.password.clone())
        }
    }

    fn load_files(&mut self) -> Task<Message> {
        let Some(sid) = self.selected_session.clone() else {
            return Task::none();
        };
        self.files_loading = true;
        let offset = self.files_offset;
        let problems = self.only_problem_files;
        let command = Command::GetFiles {
            session_id: sid.clone(),
            only_problems: problems,
            offset,
            limit: 200,
        };
        Task::perform(ipc::send(command), move |r| {
            Message::FilesLoaded(sid.clone(), offset, problems, r)
        })
    }

    fn confirm_nas_action(&mut self) -> Task<Message> {
        if self.busy {
            return Task::none();
        }
        if let (Some(sid), Some((action, file_ids))) = (
            self.selected_session.clone(),
            self.pending_nas_action.take(),
        ) {
            self.busy = true;
            return Task::perform(
                ipc::send(Command::ManageNasFiles {
                    session_id: sid,
                    file_ids,
                    action,
                }),
                Message::CommandDone,
            );
        }
        Task::none()
    }

    fn set_toast(&mut self, r: Result<Response, String>) {
        self.toast = Some(match r {
            Ok(Response::Ok { message }) => (true, message),
            Ok(_) => (true, "完了しました".into()),
            Err(e) => (false, e),
        });
    }

    fn load_history(&self) -> Task<Message> {
        Task::perform(
            async {
                let s = ipc::send(Command::ListSessions { limit: 200 }).await;
                let e = ipc::send(Command::ListEvents {
                    days: 30,
                    limit: 300,
                })
                .await;
                (s, e)
            },
            |(s, e)| Message::HistoryLoaded(s, e),
        )
    }

    /// デーモン側の通知番号が進んだら音を鳴らす（起動直後の既存通知は鳴らさない）。
    fn handle_notification(&mut self, snap: &Snapshot) {
        let seq = snap.notification_seq;
        match self.last_seq {
            None => self.last_seq = Some(seq),
            Some(prev) if seq > prev => {
                self.last_seq = Some(seq);
                if let Some(n) = &snap.last_notification {
                    let enabled = match n.kind {
                        NotificationKind::Success | NotificationKind::NasComplete => {
                            !self.settings_loaded || self.settings.play_success_sound
                        }
                        NotificationKind::Warning | NotificationKind::Error => {
                            !self.settings_loaded || self.settings.play_warning_sound
                        }
                    };
                    if enabled {
                        crate::sound::play(n.kind);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let body = match self.screen {
            Screen::Main => views::main::view(self),
            Screen::Details(i) => views::details::view(self, i),
            Screen::History => views::history::view(self),
            Screen::Settings => views::settings::view(self),
        };
        views::frame(self, body)
    }
}

pub fn file_is_actionable(f: &FileSummary) -> bool {
    f.stage_state == "DELETE_PENDING"
        || (f.stage_state == "SSD_VERIFIED"
            && matches!(
                f.nas_state.as_str(),
                "QUEUED" | "UPLOADING" | "VERIFYING" | "RETRY_WAIT" | "NAS_FAILED" | "CANCELLED"
            ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, nas_state: &str) -> FileSummary {
        FileSummary {
            file_id: id.into(),
            source_rel_path: format!("{id}.MP4"),
            destination_rel_path: format!("{id}.MP4"),
            kind: "video".into(),
            size_bytes: 100,
            stage_state: "SSD_VERIFIED".into(),
            nas_state: nas_state.into(),
            copy_attempts: 1,
            nas_attempts: 2,
            next_retry_utc: None,
            error: None,
        }
    }

    #[test]
    fn local_deletion_waits_for_confirmation_and_navigation_cancels_it() {
        let (mut app, _) = App::new();
        app.screen = Screen::History;
        app.selected_session = Some("session".into());
        app.files = vec![file("selected", "RETRY_WAIT")];
        app.selected_files.insert("selected".into());
        let _ = app.update(Message::RequestNasAction(NasFileAction::DeleteLocal));
        assert!(!app.busy, "確認前はサーバーへ削除指示を送らない");
        assert!(matches!(
            app.pending_nas_action,
            Some((NasFileAction::DeleteLocal, _))
        ));
        let _ = app.update(Message::Navigate(Screen::Main));
        let _ = app.update(Message::ConfirmNasAction);
        assert!(!app.busy, "別画面に移動した後の確認操作は無効");
        assert!(app.pending_nas_action.is_none());
    }

    #[test]
    fn stale_file_responses_never_replace_the_selected_session_or_keep_verified_selections() {
        let (mut app, _) = App::new();
        app.selected_session = Some("current".into());
        app.files = vec![file("selected", "RETRY_WAIT")];
        app.selected_files.insert("selected".into());
        let _ = app.update(Message::FilesLoaded(
            "old".into(),
            0,
            true,
            Ok(Response::Files {
                files: vec![file("wrong", "QUEUED")],
                total_files: 1,
            }),
        ));
        assert_eq!(app.files[0].file_id, "selected");
        let _ = app.update(Message::FilesLoaded(
            "current".into(),
            0,
            true,
            Ok(Response::Files {
                files: vec![file("selected", "NAS_VERIFIED")],
                total_files: 1,
            }),
        ));
        assert!(
            app.selected_files.is_empty(),
            "検証が終わったファイルは選択から外す"
        );
    }
}
