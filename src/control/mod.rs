//! 用户控制入口：CLI 与认证的会话信道。查询不会启动额外实例。

mod ipc;

use crate::core::{Command, Control, ControlLease, Direction, Launch, Service, Theme};
use anyhow::{Result, bail};
use std::sync::Arc;

pub(crate) fn register() -> Arc<dyn Control> {
    Arc::new(Channel)
}

struct Channel;
struct Route {
    args: &'static [&'static str],
    launch: Launch,
    help: &'static str,
}
const SHORTCUT: &str = "--shortcut";

// CLI 路由只声明一次，参数解析和帮助文本均由它派生。
const ROUTES: &[Route] = &[
    Route {
        args: &[],
        launch: Launch::Run(None),
        help: "常驻启动，不显示面板",
    },
    Route {
        args: &["status"],
        launch: Launch::Send(Command::Status),
        help: "查看会话、面板与固定状态",
    },
    Route {
        args: &["settings"],
        launch: Launch::Send(Command::Settings),
        help: "查看运行中的偏好",
    },
    Route {
        args: &["lookup"],
        launch: Launch::Send(Command::Lookup),
        help: "明确触发当前选区翻译",
    },
    Route {
        args: &["pin"],
        launch: Launch::Send(Command::Pin(true)),
        help: "固定，不自动查词",
    },
    Route {
        args: &["unpin"],
        launch: Launch::Send(Command::Pin(false)),
        help: "取消固定，不自动查词",
    },
    Route {
        args: &["close"],
        launch: Launch::Send(Command::Close),
        help: "关闭面板",
    },
    Route {
        args: &["quit"],
        launch: Launch::Send(Command::Quit),
        help: "退出实例",
    },
    Route {
        args: &["set", "service", "mirror"],
        launch: Launch::Send(Command::Service(Service::Mirror)),
        help: "保存镜像服务",
    },
    Route {
        args: &["set", "service", "google"],
        launch: Launch::Send(Command::Service(Service::Google)),
        help: "保存 Google 直连服务",
    },
    Route {
        args: &["set", "direction", "zh-en"],
        launch: Launch::Send(Command::Direction(Direction::ChineseEnglish)),
        help: "保存自动中英方向",
    },
    Route {
        args: &["set", "direction", "zh"],
        launch: Launch::Send(Command::Direction(Direction::Chinese)),
        help: "保存中文目标",
    },
    Route {
        args: &["set", "direction", "en"],
        launch: Launch::Send(Command::Direction(Direction::English)),
        help: "保存英文目标",
    },
    Route {
        args: &["set", "direction", "ja"],
        launch: Launch::Send(Command::Direction(Direction::Japanese)),
        help: "保存日文目标",
    },
    Route {
        args: &["set", "theme", "light"],
        launch: Launch::Send(Command::Theme(Theme::Light)),
        help: "保存浅色主题",
    },
    Route {
        args: &["set", "theme", "dark"],
        launch: Launch::Send(Command::Theme(Theme::Dark)),
        help: "保存深色主题",
    },
    Route {
        args: &["--config-path"],
        launch: Launch::ConfigPath,
        help: "查看标准配置路径，不创建文件",
    },
    Route {
        args: &["--probe-panel"],
        launch: Launch::Send(Command::ProbePanel),
        help: "公开文案面板，不读取选区、不联网",
    },
    Route {
        args: &["--check-desktop"],
        launch: Launch::CheckDesktop,
        help: "只检查原生桌面能力",
    },
    Route {
        args: &["--check-service", "mirror"],
        launch: Launch::CheckService(Service::Mirror),
        help: "镜像只翻译公开 hello",
    },
    Route {
        args: &["--check-service", "google"],
        launch: Launch::CheckService(Service::Google),
        help: "Google 只翻译公开 hello",
    },
    Route {
        args: &["--version"],
        launch: Launch::Version,
        help: "查看版本",
    },
    Route {
        args: &["--help"],
        launch: Launch::Help,
        help: "查看帮助",
    },
];

impl Control for Channel {
    fn help(&self) -> String {
        let mut lines = vec!["Glosslet — 低干扰划词翻译".to_owned(), String::new()];
        lines.extend(
            ROUTES
                .iter()
                .map(|route| format!("glosslet {:<28} {}", route.args.join(" "), route.help)),
        );
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        lines.push(format!("glosslet {SHORTCUT} Control+Alt+G  指定明确组合键"));
        lines.join("\n")
    }

    fn parse(&self, args: &[String]) -> Result<Launch> {
        let words: Vec<_> = args.iter().map(String::as_str).collect();
        if let Some(route) = ROUTES.iter().find(|route| route.args == words) {
            return Ok(route.launch.clone());
        }
        if let [SHORTCUT, shortcut] = words.as_slice() {
            #[cfg(target_os = "linux")]
            {
                let _ = shortcut;
                bail!("Linux 快捷键由桌面配置；--shortcut 仅用于 Windows/macOS");
            }
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            {
                return Ok(Launch::Run(Some((*shortcut).to_owned())));
            }
        }
        bail!("参数无效；使用 glosslet --help 查看明确的操作")
    }

    fn send(&self, command: Command) -> Result<()> {
        ipc::send(command)
    }
    fn claim(&self) -> Result<Option<Box<dyn ControlLease>>> {
        Ok(ipc::Owner::claim()?.map(|owner| Box::new(owner) as Box<dyn ControlLease>))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_intents_are_closed_and_do_not_accept_arbitrary_services() {
        for (args, expected) in [
            (vec!["settings"], Command::Settings),
            (vec!["pin"], Command::Pin(true)),
            (vec!["unpin"], Command::Pin(false)),
            (vec!["lookup"], Command::Lookup),
            (vec!["set", "theme", "light"], Command::Theme(Theme::Light)),
            (
                vec!["set", "direction", "ja"],
                Command::Direction(Direction::Japanese),
            ),
        ] {
            let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
            assert!(
                matches!(Channel.parse(&args).unwrap(), Launch::Send(command) if command == expected)
            );
        }
        for args in [
            vec!["lookup", "extra"],
            vec!["pin", "true"],
            vec!["set", "service", "https://unknown.test"],
            vec!["set", "direction", "automatic"],
        ] {
            assert!(
                Channel
                    .parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
                    .is_err()
            );
        }
    }
    #[test]
    fn routes_are_unique_and_help_has_no_separate_registration_list() {
        for (index, route) in ROUTES.iter().enumerate() {
            assert!(
                ROUTES[..index]
                    .iter()
                    .all(|previous| previous.args != route.args)
            );
            assert!(
                Channel
                    .parse(
                        &route
                            .args
                            .iter()
                            .map(|arg| (*arg).to_owned())
                            .collect::<Vec<_>>()
                    )
                    .is_ok()
            );
            assert!(Channel.help().contains(route.help));
        }
    }
}
