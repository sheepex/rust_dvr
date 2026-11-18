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
    directories::ProjectDirs::from("br", "visor-dvr", "visor-dvr")
        .map(|dirs| dirs.config_dir().join(FILE_NAME))
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
pub fn load_local<T: DeserializeOwned>(path: &Path) -> Result<(Config<Vec<T>>, bool), String> {
    match std::fs::read(path) {
        Ok(data) => Ok((parse(&Zeroizing::new(data))?, false)),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err("Não foi possível abrir configs.json.".into())
        }
        Err(_) => match std::fs::read(path.with_file_name(LEGACY_NAME)) {
            Ok(data) => Ok((parse(&Zeroizing::new(data))?, true)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok((Config::new(Vec::new(), DEFAULT_CACHE_SECONDS), false))
            }
            Err(_) => Err("Não foi possível abrir o cadastro anterior.".into()),
        },
    }
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
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    write_new(&temporary, &data).map_err(|_| "Não foi possível salvar a configuração.")?;
    let result = if create_only {
        // Publish only a complete file, without overwriting a concurrent migration.
        std::fs::hard_link(&temporary, path)
    } else {
        std::fs::rename(&temporary, path)
    };
    if create_only || result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|_| "Não foi possível atualizar o arquivo de configuração.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_keeps_legacy_and_never_overwrites_the_new_file() {
        let dir = std::env::temp_dir().join(format!("visor-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let legacy = dir.join(LEGACY_NAME);
        let path = dir.join(FILE_NAME);
        let original = br#"{"playback_cache_seconds":11,"equipment":["a","b"]}"#;
        std::fs::write(&legacy, original).unwrap();
        let (settings, from_legacy) = load_local::<String>(&path).unwrap();
        assert!(from_legacy);
        migrate(&path, &settings).unwrap();
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
        let (settings, from_legacy) = load_local::<String>(&path).unwrap();
        assert!(!from_legacy);
        assert_eq!(settings.equipment, ["a", "b"]);
        assert_eq!(settings.playback_cache_seconds, 11);
        assert!(migrate(&path, &Config::new(["other"], 7)).is_err());
        assert!(export(&path, &Config::new(["other"], 7)).is_err());

        std::fs::write(&path, b"invalid").unwrap();
        assert!(load_local::<String>(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
