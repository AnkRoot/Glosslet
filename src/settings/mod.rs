//! 用户设置：标准目录、严格文件契约、原子替换。只保存偏好，不保存选区。

use crate::core::{Preferences, Settings};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub(crate) fn register() -> Arc<dyn Preferences> {
    Arc::new(Store)
}

impl Preferences for Store {
    fn path(&self) -> Result<PathBuf> {
        Store::path()
    }
    fn load(&self) -> Result<Settings> {
        Store::load()
    }
    fn save(&self, settings: Settings) -> Result<()> {
        let path = Store::path()?;
        Store::write(settings, &path).with_context(|| format!("无法保存设置 {}", path.display()))
    }
}

struct Store;

impl Store {
    const MAX_BYTES: usize = 4096;

    fn path() -> Result<PathBuf> {
        #[cfg(target_os = "linux")]
        let directory = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
        {
            Some(root) => PathBuf::from(root),
            None => PathBuf::from(std::env::var_os("HOME").context("未设置 HOME")?).join(".config"),
        }
        .join(env!("CARGO_PKG_NAME"));
        #[cfg(target_os = "windows")]
        let directory =
            PathBuf::from(std::env::var_os("APPDATA").context("未设置 APPDATA")?).join("Glosslet");
        #[cfg(target_os = "macos")]
        let directory = PathBuf::from(std::env::var_os("HOME").context("未设置 HOME")?)
            .join("Library/Application Support/Glosslet");
        if !directory.is_absolute() {
            bail!("设置根目录必须为绝对路径");
        }
        Ok(directory.join("settings.json"))
    }

    fn load() -> Result<Settings> {
        let path = Self::path()?;
        Self::read(&path).with_context(|| format!("无法加载设置 {}", path.display()))
    }

    fn directory(path: &Path) -> Result<&Path> {
        if !path.is_absolute() {
            bail!("设置路径必须为绝对路径");
        }
        let directory = path.parent().context("设置路径缺少目录")?;
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() => Self::private(&metadata)?,
            Ok(_) => bail!("设置目录必须为真实目录，不接受符号链接"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(directory)
    }

    fn private(metadata: &Metadata) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0
            {
                bail!("设置目录和文件必须属于当前用户，且不向其他用户开放权限");
            }
        }
        #[cfg(windows)]
        let _ = metadata;
        Ok(())
    }

    fn read(path: &Path) -> Result<Settings> {
        Self::directory(path)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => Self::private(&metadata)?,
            Ok(_) => bail!("设置文件必须为普通文件，不接受符号链接"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            );
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("设置文件必须为普通文件");
        }
        Self::private(&metadata)?;
        let mut bytes = Vec::new();
        file.take(Self::MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > Self::MAX_BYTES {
            bail!("设置文件超过 4096 字节");
        }
        serde_json::from_slice(&bytes).context("设置格式无效；不恢复默认、不读取或迁移旧配置")
    }

    fn write(settings: Settings, path: &Path) -> Result<()> {
        let directory = Self::directory(path)?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory)?;
        Self::directory(path)?;
        Self::read(path).context("现有设置文件无效，拒绝覆盖")?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temporary = directory.join(format!(
            ".settings-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let result = (|| -> Result<()> {
            let mut bytes = serde_json::to_vec_pretty(&settings)?;
            bytes.push(b'\n');
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path).context("无法原子保存设置")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Direction, Service, Theme};

    struct SettingsFile {
        directory: PathBuf,
    }
    impl SettingsFile {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "glosslet-settings-test-{}-{nonce}",
                std::process::id()
            ));
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&directory).unwrap();
            Self { directory }
        }
        fn path(&self) -> PathBuf {
            self.directory.join("glosslet/settings.json")
        }
    }
    impl Drop for SettingsFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn missing_file_uses_defaults_without_creating_data() {
        let file = SettingsFile::new();
        assert_eq!(Store::read(&file.path()).unwrap(), Settings::default());
        assert!(!file.path().parent().unwrap().exists());
    }

    #[test]
    fn schema_is_closed_and_complete() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"service":"mirror", "direction":"chinese_english"}),
            serde_json::json!({"service":"other", "direction":"chinese_english", "theme":"dark"}),
            serde_json::json!({"service":"mirror", "direction":"auto", "theme":"dark"}),
            serde_json::json!({"service":"mirror", "direction":"chinese_english", "theme":"dark", "history":[]}),
            serde_json::json!({"language":"zh", "mirror":"https://unknown.test"}),
            serde_json::Value::Null,
        ] {
            assert!(serde_json::from_value::<Settings>(value).is_err());
        }
    }

    #[test]
    fn writes_are_private_atomic_and_readable_after_restart() {
        let file = SettingsFile::new();
        let path = file.path();
        for theme in [Theme::Light, Theme::Dark] {
            let value = Settings {
                service: Service::Google,
                direction: Direction::Japanese,
                theme,
            };
            Store::write(value, &path).unwrap();
            assert_eq!(Store::read(&path).unwrap(), value);
            assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        }
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.as_object().unwrap().len(), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
                0o700
            );
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn invalid_and_oversized_files_are_not_reset_or_overwritten() {
        let file = SettingsFile::new();
        let path = file.path();
        Store::write(Settings::default(), &path).unwrap();
        for bytes in [
            Vec::new(),
            b"{\"service\":\"mirror\"".to_vec(),
            b"[]".to_vec(),
            vec![b' '; Store::MAX_BYTES + 1],
        ] {
            fs::write(&path, &bytes).unwrap();
            assert!(Store::read(&path).is_err());
            assert!(Store::write(Settings::default(), &path).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        }
    }

    #[test]
    fn readers_never_observe_partial_replacements() {
        let file = SettingsFile::new();
        let path = file.path();
        Store::write(Settings::default(), &path).unwrap();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            for _ in 0..512 {
                assert!(Store::read(&reader_path).is_ok());
            }
        });
        for _ in 0..16 {
            for theme in [Theme::Light, Theme::Dark] {
                Store::write(
                    Settings {
                        theme,
                        ..Settings::default()
                    },
                    &path,
                )
                .unwrap();
            }
        }
        reader.join().unwrap();
    }

    #[test]
    fn relative_and_non_file_paths_are_rejected() {
        assert!(Store::read(Path::new("settings.json")).is_err());
        let file = SettingsFile::new();
        let path = file.directory.join("directory.json");
        fs::create_dir(&path).unwrap();
        assert!(Store::read(&path).is_err());
        assert!(Store::write(Settings::default(), &path).is_err());
        let blocker = file.directory.join("not-a-directory");
        fs::write(&blocker, b"untouched").unwrap();
        assert!(Store::write(Settings::default(), &blocker.join("settings.json")).is_err());
        assert_eq!(fs::read(&blocker).unwrap(), b"untouched");
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_links_and_permissions_do_not_modify_targets() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let file = SettingsFile::new();
        let path = file.path();
        Store::write(Settings::default(), &path).unwrap();
        let original = fs::read(&path).unwrap();
        let link = file.directory.join("linked.json");
        symlink(&path, &link).unwrap();
        assert!(Store::read(&link).is_err());
        assert!(Store::write(Settings::default(), &link).is_err());
        let directory = file.directory.join("linked-directory");
        symlink(path.parent().unwrap(), &directory).unwrap();
        assert!(Store::read(&directory.join("settings.json")).is_err());
        assert!(Store::write(Settings::default(), &directory.join("settings.json")).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Store::read(&path).is_err());
        assert!(Store::write(Settings::default(), &path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Store::read(&path).is_err());
        assert!(Store::write(Settings::default(), &path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}
