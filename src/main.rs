#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod core;

// 功能入口与装配只在这里声明一次；各入口返回 core 契约，不相互导入实现。
macro_rules! features {
    ($($module:ident => $port:ident),+ $(,)?) => {
        $(mod $module;)+
        fn assemble() -> core::Services {
            core::Services { $($port: $module::register()),+ }
        }
    };
}

features! {
    control => control,
    settings => settings,
    translation => translation,
    panel => panel,
    lookup => lookup,
}

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    if std::env::args_os().nth(1).is_some() {
        // SAFETY: 只连接调用者已有控制台；常驻启动不新建控制台。
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
    }
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Glosslet：{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let services = assemble();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match services.control.parse(&args)? {
        core::Launch::Run(shortcut) => (services.lookup.run)(services, shortcut.as_deref()),
        core::Launch::Send(command) => services.control.send(command),
        core::Launch::Help => {
            println!("{}", services.control.help());
            Ok(())
        }
        core::Launch::Version => {
            println!("Glosslet {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        core::Launch::ConfigPath => {
            println!("{}", services.settings.path()?.display());
            Ok(())
        }
        core::Launch::CheckDesktop => (services.lookup.check_desktop)(),
        core::Launch::CheckService(service) => (services.lookup.check_service)(services, service),
    }
}
