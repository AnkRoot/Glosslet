//! 共享契约：数据、端口和装配值。这里不依赖任何功能实现。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fmt, future::Future, io, path::PathBuf, pin::Pin, sync::Arc};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Service {
    #[default]
    Mirror,
    Google,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    #[default]
    ChineseEnglish,
    Chinese,
    English,
    Japanese,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Theme {
    Light,
    #[default]
    Dark,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub(crate) service: Service,
    pub(crate) direction: Direction,
    pub(crate) theme: Theme,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "command",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Command {
    Status,
    Settings,
    Lookup,
    ProbePanel,
    Pin(bool),
    Close,
    Quit,
    Service(Service),
    Direction(Direction),
    Theme(Theme),
}

pub(crate) const MAX_SELECTION_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Ticket(pub(crate) u64);

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub(crate) ticket: Ticket,
    pub(crate) source: Arc<str>,
    pub(crate) settings: Settings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    CaptureUnavailable,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    PermissionRequired,
    CaptureTimeout,
    SelectionTooLarge,
    InvalidText,
    Network,
    Timeout,
    RateLimited,
    Http(u16),
    InvalidResponse,
    ResponseTooLarge,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CaptureUnavailable => "当前选区不可读取",
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Self::PermissionRequired => "请允许辅助功能访问；不模拟复制取词",
            Self::CaptureTimeout => "取词超时，请重新触发",
            Self::SelectionTooLarge => "选区过长，请缩小范围",
            Self::InvalidText => "选区不是有效文本",
            Self::Network => "网络不可达，点击重试",
            Self::Timeout => "翻译超时，点击重试",
            Self::RateLimited => "服务限流，请稍后重试",
            Self::Http(_) => "翻译服务异常，点击重试",
            Self::InvalidResponse => "翻译响应无效，点击重试",
            Self::ResponseTooLarge => "翻译响应过大",
        })
    }
}

impl std::error::Error for Failure {}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum Translation {
    Sentence(String),
    Dictionary(Vec<DictionaryEntry>),
    NoResult,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct DictionaryEntry {
    #[serde(default)]
    pub(crate) pos: String,
    #[serde(default)]
    pub(crate) terms: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisplayIdentity {
    pub(crate) ticket: Ticket,
    pub(crate) revision: u64,
}

#[derive(Debug)]
pub(crate) enum Content {
    Loading,
    Ready(Translation),
    Failed {
        error: Failure,
        retry: Option<Request>,
    },
}

#[derive(Debug)]
pub(crate) struct Display {
    pub(crate) ticket: Ticket,
    pub(crate) content: Content,
    pub(crate) revision: u64,
}

impl Display {
    pub(crate) fn new(ticket: Ticket, content: Content) -> Self {
        Self {
            ticket,
            content,
            revision: 0,
        }
    }

    pub(crate) fn identity(&self) -> DisplayIdentity {
        DisplayIdentity {
            ticket: self.ticket,
            revision: self.revision,
        }
    }

    pub(crate) fn replace(&mut self, content: Content) {
        self.content = content;
        self.revision += 1;
    }
}

pub(crate) const PANEL_WIDTH: f64 = 256.0;
pub(crate) const PANEL_HEIGHT: f64 = 100.0;
pub(crate) const PANEL_HEADER: f64 = 20.0;

#[derive(Serialize)]
pub(crate) struct Line {
    pub text: String,
    pub small: bool,
    pub copy: bool,
}

#[derive(Serialize)]
pub(crate) struct View {
    pub identity: DisplayIdentity,
    pub theme: Theme,
    pub pinned: bool,
    pub retry: bool,
    pub lines: Vec<Line>,
}

pub(crate) struct Term {
    pub top: f64,
    pub bottom: f64,
    pub text: String,
}

pub(crate) struct Image {
    pub pixels: Vec<u32>,
    pub height: f64,
    pub terms: Vec<Term>,
}

pub(crate) trait Painter {
    fn view(&self, display: &Display, pinned: bool, theme: Theme) -> View;
    fn place(&self, point: (f64, f64), bounds: (f64, f64, f64, f64)) -> Option<(f64, f64)>;
    fn render(&mut self, view: &View, size: (u32, u32), scale: f64, scroll: f64) -> Image;
    fn reset(&mut self);
}

pub(crate) trait Preferences: Send + Sync {
    fn path(&self) -> Result<PathBuf>;
    fn load(&self) -> Result<Settings>;
    fn save(&self, settings: Settings) -> Result<()>;
}

pub(crate) type TranslationFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<Translation, Failure>> + Send + 'a>>;

pub(crate) trait Translator: Send + Sync {
    fn translate<'a>(&'a self, request: &'a Request) -> TranslationFuture<'a>;
}

pub(crate) struct ControlRequest {
    pub command: Command,
    pub reply: tokio::sync::oneshot::Sender<std::result::Result<String, String>>,
}

pub(crate) type ControlSink = Arc<dyn Fn(ControlRequest) + Send + Sync>;
pub(crate) type ControlFuture = Pin<Box<dyn Future<Output = io::Result<()>> + Send>>;

pub(crate) trait ControlServer: Send {
    fn serve(self: Box<Self>, send: ControlSink) -> ControlFuture;
}

pub(crate) trait ControlLease: Send {
    fn bind(self: Box<Self>) -> Result<Box<dyn ControlServer>>;
}

#[derive(Clone)]
pub(crate) enum Launch {
    Run(Option<String>),
    Send(Command),
    Help,
    Version,
    ConfigPath,
    CheckDesktop,
    CheckService(Service),
}

pub(crate) trait Control: Send + Sync {
    fn help(&self) -> String;
    fn parse(&self, args: &[String]) -> Result<Launch>;
    fn send(&self, command: Command) -> Result<()>;
    fn claim(&self) -> Result<Option<Box<dyn ControlLease>>>;
}

#[derive(Clone, Copy)]
pub(crate) struct LookupModule {
    pub run: fn(Services, Option<&str>) -> Result<()>,
    pub check_desktop: fn() -> Result<()>,
    pub check_service: fn(Services, Service) -> Result<()>,
}

#[derive(Clone)]
pub(crate) struct Services {
    pub control: Arc<dyn Control>,
    pub settings: Arc<dyn Preferences>,
    pub translation: fn() -> Result<Arc<dyn Translator>>,
    pub panel: fn() -> Box<dyn Painter>,
    pub lookup: LookupModule,
}
