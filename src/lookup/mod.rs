//! 查词功能入口与桌面装配。状态机和偏好控制共享，后端只适配原生事件与资源。

mod controller;
#[cfg(target_os = "linux")]
mod gnome;
mod native;
mod session;
#[cfg(target_os = "linux")]
mod wayland;

use crate::core::{LookupModule, Request, Service, Services, Settings, Ticket};
use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::{Context, bail};
use std::{sync::Arc, time::Duration};

pub(crate) fn register() -> LookupModule {
    LookupModule {
        run,
        check_desktop,
        check_service,
    }
}

fn run(services: Services, shortcut: Option<&str>) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = shortcut;
        match linux_session()?.as_str() {
            "wayland" if gnome_session()? => gnome::run(services),
            "wayland" => wayland::run(services),
            "x11" => native::run(services, None),
            _ => unreachable!(),
        }
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        native::run(services, shortcut)
    }
}

fn check_desktop() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        match linux_session()?.as_str() {
            "wayland" if gnome_session()? => gnome::check(),
            "wayland" => wayland::check_desktop(),
            "x11" => native::check_desktop(),
            _ => unreachable!(),
        }
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        native::check_desktop()
    }
}

fn check_service(services: Services, service: Service) -> Result<()> {
    let translator = (services.translation)()?;
    let request = Request {
        ticket: Ticket(1),
        source: Arc::from("hello"),
        settings: Settings {
            service,
            ..Settings::default()
        },
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    let started = std::time::Instant::now();
    let result = runtime.block_on(translator.translate(&request));
    runtime.shutdown_timeout(Duration::from_secs(1));
    println!("{}", serde_json::to_string(&result?)?);
    println!("耗时：{}ms", started.elapsed().as_millis());
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_session() -> Result<String> {
    let session = std::env::var("XDG_SESSION_TYPE").context("未设置 XDG_SESSION_TYPE")?;
    if !matches!(session.as_str(), "wayland" | "x11") {
        bail!("会话必须明确为 Wayland 或 X11；不自动切换显示服务");
    }
    Ok(session)
}

#[cfg(target_os = "linux")]
fn gnome_session() -> Result<bool> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").context("未设置 XDG_CURRENT_DESKTOP")?;
    let names: Vec<_> = desktop.split(':').map(str::to_ascii_lowercase).collect();
    let gnome = names.iter().any(|name| name == "gnome");
    if gnome
        && names
            .iter()
            .any(|name| matches!(name.as_str(), "niri" | "hyprland" | "sway" | "kde"))
    {
        bail!("桌面身份不唯一，拒绝混用 compositor 上下文");
    }
    Ok(gnome)
}
