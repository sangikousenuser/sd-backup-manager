use crate::ipc::{self, DaemonEvent};
use crate::views;
use backup_ipc::{
    Command, EventSummary, FileSummary, NasTestReport, NotificationKind, Response, SessionSummary,
    SettingsView, Snapshot,
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
    FilesLoaded(Result<Response, String>),
    RestoreDest(String),
    RestoreSession(String),
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
        Subscription::run(ipc::subscribe).map(Message::Daemon)
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
                self.selected_session = Some(id.clone());
                self.files.clear();
                return Task::perform(
                    ipc::send(Command::GetFiles {
                        session_id: id,
                        only_problems: true,
                    }),
                    Message::FilesLoaded,
                );
            }
            Message::FilesLoaded(r) => {
                if let Ok(Response::Files { files }) = r {
                    self.files = files;
                }
            }
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
