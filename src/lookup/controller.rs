//! 各桌面共享的查词控制：状态、偏好修改和回复语义只有这一份。

use super::session::Session;
use crate::core::{Command, Preferences, Settings, Theme, Ticket};
use anyhow::{Context, Result};
use std::sync::Arc;

pub(super) struct Controller {
    pub session: Session,
    settings: Settings,
    preferences: Arc<dyn Preferences>,
}

pub(super) struct Configured {
    pub reply: String,
    pub redraw: bool,
}

impl Controller {
    pub fn new(preferences: Arc<dyn Preferences>) -> Result<Self> {
        let settings = preferences.load()?;
        Ok(Self {
            session: Session::default(),
            settings,
            preferences,
        })
    }

    pub fn settings(&self) -> Settings {
        self.settings
    }

    pub fn capture(&mut self) -> Ticket {
        self.session.capture(self.settings)
    }

    pub fn configure(&mut self, command: &Command) -> Result<Option<Configured>> {
        if *command == Command::Settings {
            return Ok(Some(Configured {
                reply: serde_json::to_string_pretty(&self.settings)?,
                redraw: false,
            }));
        }
        let mut updated = self.settings;
        match command {
            Command::Service(service) => updated.service = *service,
            Command::Direction(direction) => updated.direction = *direction,
            Command::Theme(theme) => updated.theme = *theme,
            _ => return Ok(None),
        }
        self.preferences
            .save(updated)
            .context("设置未保存；运行值未改变")?;
        self.settings = updated;
        Ok(Some(Configured {
            reply: "已保存".into(),
            redraw: matches!(command, Command::Theme(_)),
        }))
    }

    pub fn toggle_theme(&mut self) -> Result<()> {
        let next = if self.settings.theme == Theme::Dark {
            Theme::Light
        } else {
            Theme::Dark
        };
        self.configure(&Command::Theme(next))?;
        Ok(())
    }

    pub fn status(&self, desktop: &str, extra: Option<&str>) -> String {
        format!(
            "桌面：{desktop}；阶段：{}{}；面板：{}；固定：{}",
            self.session.phase(),
            extra.map(|text| format!("；{text}")).unwrap_or_default(),
            if self.session.display().is_some() {
                "显示中"
            } else {
                "隐藏"
            },
            self.session.pinned()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Direction, Service};
    use std::path::PathBuf;

    struct ReadOnly;
    impl Preferences for ReadOnly {
        fn path(&self) -> Result<PathBuf> {
            Ok(PathBuf::from("unused"))
        }
        fn load(&self) -> Result<Settings> {
            Ok(Settings::default())
        }
        fn save(&self, _: Settings) -> Result<()> {
            anyhow::bail!("测试写入失败")
        }
    }

    #[test]
    fn failed_settings_writes_never_mutate_runtime_values_or_start_lookup() {
        let mut controller = Controller::new(Arc::new(ReadOnly)).unwrap();
        for command in [
            Command::Service(Service::Google),
            Command::Direction(Direction::Japanese),
            Command::Theme(Theme::Light),
        ] {
            assert!(controller.configure(&command).is_err());
            assert_eq!(controller.settings(), Settings::default());
            assert!(controller.session.display().is_none());
            assert!(controller.session.capture_ticket().is_none());
        }
        let query = controller.configure(&Command::Settings).unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Settings>(&query.reply).unwrap(),
            Settings::default()
        );
        assert!(!query.redraw);
        assert!(controller.configure(&Command::Lookup).unwrap().is_none());
    }
}
