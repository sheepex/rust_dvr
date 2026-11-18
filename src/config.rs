use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MIN_CACHE_SECONDS: u16 = 1;
pub const MAX_CACHE_SECONDS: u16 = 30;
pub const DEFAULT_CACHE_SECONDS: u16 = 7;

fn default_cache_seconds() -> u16 {
    DEFAULT_CACHE_SECONDS
}

#[derive(Serialize, Deserialize)]
pub struct Config<T> {
    #[serde(default = "default_cache_seconds")]
    pub playback_cache_seconds: u16,
    pub equipment: T,
}

impl<T> Config<T> {
    pub fn new(equipment: T, playback_cache_seconds: u16) -> Self {
        Self {
            playback_cache_seconds,
            equipment,
        }
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
        .map(|dirs| dirs.config_dir().join("equipamentos.json"))
}

pub fn load<T: DeserializeOwned>(path: &Path) -> Result<Config<Vec<T>>, String> {
    match std::fs::read(path) {
        Ok(data) => match serde_json::from_slice(&data) {
            Ok(ConfigFile::Legacy(equipment)) => Ok(Config::new(equipment, DEFAULT_CACHE_SECONDS)),
            Ok(ConfigFile::Current(mut config)) => {
                config.playback_cache_seconds = config
                    .playback_cache_seconds
                    .clamp(MIN_CACHE_SECONDS, MAX_CACHE_SECONDS);
                Ok(config)
            }
            Err(_) => Err("Não foi possível ler o arquivo de configuração.".into()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Config::new(Vec::new(), DEFAULT_CACHE_SECONDS))
        }
        Err(_) => Err("Não foi possível abrir o arquivo de configuração.".into()),
    }
}

pub fn save<T: Serialize>(path: &Path, config: &Config<T>) -> Result<(), String> {
    use std::io::Write;

    let parent = path.parent().ok_or("Pasta de configuração inválida.")?;
    std::fs::create_dir_all(parent)
        .map_err(|_| "Não foi possível criar a pasta de configuração.")?;
    let data = serde_json::to_vec_pretty(config).map_err(|_| "Erro ao preparar a configuração.")?;
    // Stage on the same filesystem so a failed write leaves the existing file intact.
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| "Não foi possível salvar a configuração.")?;
    let written = file.write_all(&data).and_then(|()| file.sync_all());
    drop(file);
    if written
        .and_then(|()| std::fs::rename(&temporary, path))
        .is_err()
    {
        let _ = std::fs::remove_file(&temporary);
        return Err("Não foi possível atualizar o arquivo de configuração.".into());
    }
    Ok(())
}
