use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub type Passwords = std::collections::HashMap<String, Zeroizing<String>>;

pub const MIN_CACHE_SECONDS: u16 = 1;
pub const MAX_CACHE_SECONDS: u16 = 30;
pub const DEFAULT_CACHE_SECONDS: u16 = 7;
pub const FILE_NAME: &str = "configs.json";
const LEGACY_NAME: &str = "equipamentos.json";

fn default_cache_seconds() -> u16 {
    DEFAULT_CACHE_SECONDS
}

#[derive(Serialize, Deserialize)]
pub struct Config<T> {
    #[serde(default = "default_cache_seconds")]
    pub playback_cache_seconds: u16,
    pub equipment: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passwords: Option<Passwords>,
}

impl<T> Config<T> {
    pub fn new(equipment: T, playback_cache_seconds: u16) -> Self {
        Self {
            playback_cache_seconds,
            equipment,
            passwords: None,
        }
    }
}

pub fn legacy_password(id: &str) -> Result<Option<String>, &'static str> {
    let entry = keyring::Entry::new("br.visor-dvr.equipamentos", id)
        .map_err(|_| "Armazenamento seguro do sistema indisponível.")?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err("Não foi possível migrar as senhas do armazenamento seguro. Desbloqueie-o e tente novamente."),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ConfigFile<T> {
    Legacy(Vec<T>),
    Current(Config<Vec<T>>),
}

pub fn path() -> Option<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    return std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name(FILE_NAME));
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    previous_path()
}

pub fn previous_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("br", "visor-dvr", "visor-dvr")
        .map(|dirs| dirs.config_dir().join(FILE_NAME))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn installed_dir() -> Option<PathBuf> {
    let dirs = directories::BaseDirs::new()?;
    let application = if cfg!(target_os = "windows") {
        "RustDVR"
    } else {
        "rustdvr"
    };
    Some(dirs.data_local_dir().join(application))
}

pub fn parse<T: DeserializeOwned>(data: &[u8]) -> Result<Config<Vec<T>>, String> {
    match serde_json::from_slice(data) {
        Ok(ConfigFile::Legacy(equipment)) => Ok(Config::new(equipment, DEFAULT_CACHE_SECONDS)),
        Ok(ConfigFile::Current(mut config)) => {
            config.playback_cache_seconds = config
                .playback_cache_seconds
                .clamp(MIN_CACHE_SECONDS, MAX_CACHE_SECONDS);
            Ok(config)
        }
        Err(_) => Err("Não foi possível ler o arquivo de configuração.".into()),
    }
}

pub fn read_import<T: DeserializeOwned>(path: &Path) -> Result<Config<Vec<T>>, String> {
    let data = std::fs::read(path)
        .map(Zeroizing::new)
        .map_err(|_| "Não foi possível abrir o arquivo para importar.")?;
    parse(&data)
}

/// Fall back only if the canonical file is absent, not if it is unreadable or malformed.
fn find_local<T: DeserializeOwned>(path: &Path) -> Result<Option<(Config<Vec<T>>, bool)>, String> {
    match std::fs::read(path) {
        Ok(data) => Ok(Some((parse(&Zeroizing::new(data))?, false))),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err("Não foi possível abrir configs.json.".into())
        }
        Err(_) => match std::fs::read(path.with_file_name(LEGACY_NAME)) {
            Ok(data) => Ok(Some((parse(&Zeroizing::new(data))?, true))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("Não foi possível abrir o cadastro anterior.".into()),
        },
    }
}

pub fn load_local<T: DeserializeOwned>(path: &Path) -> Result<(Config<Vec<T>>, bool), String> {
    let previous = previous_path();
    load_local_with_fallbacks(path, previous.as_slice())
}

pub fn load_local_with_fallbacks<T: DeserializeOwned>(
    path: &Path,
    fallbacks: &[PathBuf],
) -> Result<(Config<Vec<T>>, bool), String> {
    if let Some(local) = find_local(path)? {
        return Ok(local);
    }
    for previous in fallbacks
        .iter()
        .filter(|previous| previous.as_path() != path)
    {
        if let Some((config, _)) = find_local(previous)? {
            return Ok((config, true));
        }
    }
    Ok((Config::new(Vec::new(), DEFAULT_CACHE_SECONDS), false))
}

fn prepare_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("Pasta de configuração inválida.")?;
    std::fs::create_dir_all(parent)
        .map_err(|_| "Não foi possível criar a pasta de configuração.".into())
}

fn write_new(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let written = file.write_all(data).and_then(|()| file.sync_all());
    drop(file);
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn copy_for_install(source: &Path, destination: &Path) -> std::io::Result<bool> {
    if destination.try_exists()? || destination.with_file_name(LEGACY_NAME).try_exists()? {
        return Ok(false);
    }
    let contents = match std::fs::read(source) {
        Ok(contents) => Zeroizing::new(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    match write_atomic(destination, &contents, true) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn export<T: Serialize>(path: &Path, config: &T) -> Result<(), String> {
    let data = Zeroizing::new(
        serde_json::to_vec_pretty(config).map_err(|_| "Erro ao preparar a configuração.")?,
    );
    write_new(path, &data).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            "O arquivo já existe. Escolha outro nome para exportar."
        } else {
            "Não foi possível exportar a configuração."
        }
        .into()
    })
}

pub fn backup_current(path: &Path) -> Result<(), String> {
    let data = match std::fs::read(path) {
        Ok(data) => Zeroizing::new(data),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {
            return Err("Não foi possível ler a configuração atual para fazer o backup.".into())
        }
    };
    let backup = path.with_file_name(format!("configs.{}.bak.json", uuid::Uuid::new_v4()));
    write_new(&backup, &data)
        .map_err(|_| "Não foi possível fazer o backup da configuração atual.".into())
}

pub fn save<T: Serialize>(path: &Path, config: &Config<T>) -> Result<(), String> {
    save_atomic(path, config, false)
}

pub fn migrate<T: Serialize>(path: &Path, config: &Config<T>) -> Result<(), String> {
    save_atomic(path, config, true)
}

fn save_atomic<T: Serialize>(
    path: &Path,
    config: &Config<T>,
    create_only: bool,
) -> Result<(), String> {
    prepare_parent(path)?;
    let data = Zeroizing::new(
        serde_json::to_vec_pretty(config).map_err(|_| "Erro ao preparar a configuração.")?,
    );
    write_atomic(path, &data, create_only)
        .map_err(|_| "Não foi possível atualizar o arquivo de configuração.".into())
}

fn write_atomic(path: &Path, data: &[u8], create_only: bool) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    write_new(&temporary, data)?;
    let result = if create_only {
        // Publish only a complete file, without overwriting a concurrent migration.
        std::fs::hard_link(&temporary, path)
    } else {
        std::fs::rename(&temporary, path)
    };
    if create_only || result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn migrates_the_previous_profile_and_keeps_an_existing_installed_config() {
        let dir = tempfile::tempdir().unwrap();
        let previous = dir.path().join("previous").join(FILE_NAME);
        let portable = dir.path().join(FILE_NAME);
        let installed = dir.path().join("installed").join(FILE_NAME);
        save(&previous, &Config::new(["previous"], 7)).unwrap();
        std::fs::create_dir(installed.parent().unwrap()).unwrap();
        let (settings, needs_migration) =
            load_local_with_fallbacks::<String>(&portable, std::slice::from_ref(&previous))
                .unwrap();
        assert!(needs_migration);
        migrate(&portable, &settings).unwrap();
        assert!(copy_for_install(&portable, &installed).unwrap());
        save(&installed, &Config::new(["installed"], 8)).unwrap();
        assert!(!copy_for_install(&portable, &installed).unwrap());

        let (settings, needs_migration) =
            load_local_with_fallbacks::<String>(&installed, &[previous]).unwrap();
        assert!(!needs_migration);
        assert_eq!(settings.equipment, ["installed"]);
    }

    #[test]
    fn migration_keeps_legacy_and_never_overwrites_the_new_file() {
        let dir = std::env::temp_dir().join(format!("visor-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let legacy = dir.join(LEGACY_NAME);
        let path = dir.join(FILE_NAME);
        let original = br#"{"playback_cache_seconds":11,"equipment":["a","b"]}"#;
        std::fs::write(&legacy, original).unwrap();
        let (settings, from_legacy) = load_local_with_fallbacks::<String>(&path, &[]).unwrap();
        assert!(from_legacy);
        migrate(&path, &settings).unwrap();
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
        let (settings, from_legacy) = load_local_with_fallbacks::<String>(&path, &[]).unwrap();
        assert!(!from_legacy);
        assert_eq!(settings.equipment, ["a", "b"]);
        assert_eq!(settings.playback_cache_seconds, 11);
        assert!(migrate(&path, &Config::new(["other"], 7)).is_err());
        assert!(export(&path, &Config::new(["other"], 7)).is_err());

        std::fs::write(&path, b"invalid").unwrap();
        assert!(load_local_with_fallbacks::<String>(&path, &[]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
