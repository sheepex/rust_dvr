use super::{apply_update, launch, Download, Result};
use directories::BaseDirs;
use std::{
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const DESKTOP: &str = include_str!("../../packaging/linux/visor-dvr.desktop");
const ICON: &[u8] = include_bytes!("../../packaging/linux/visor-dvr.svg");

fn base_dirs() -> Result<BaseDirs> {
    BaseDirs::new().ok_or_else(|| "Não foi possível identificar as pastas do usuário.".into())
}

pub(super) fn install_path() -> Result<PathBuf> {
    Ok(crate::config::installed_dir()
        .ok_or("Não foi possível identificar a pasta de instalação do usuário.")?
        .join("rustdvr"))
}

pub(super) fn set_executable(file: &File) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(0o755))
}

fn desktop_entry(target: &Path) -> Result<String> {
    let value = target
        .to_str()
        .filter(|value| !value.contains(['=', '\n', '\r']))
        .ok_or("O caminho do executável não é compatível com atalhos .desktop.")?;
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(ch);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    Ok(DESKTOP.replace("Exec=rustdvr\n", &format!("Exec={quoted}\n")))
}

fn write_file(path: &Path, contents: &[u8]) -> Result<()> {
    let directory = super::parent(path)?;
    fs::create_dir_all(directory)?;
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(contents)?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    staged.as_file().sync_all()?;
    staged.persist(path)?;
    Ok(())
}

pub(super) fn write_shortcuts(target: &Path) -> Result<()> {
    let desktop = desktop_entry(target)?;
    let dirs = base_dirs()?;
    let data = dirs.data_dir();
    write_file(
        &data.join("icons/hicolor/scalable/apps/visor-dvr.svg"),
        ICON,
    )?;
    write_file(
        &data.join("applications/visor-dvr.desktop"),
        desktop.as_bytes(),
    )
}

pub(super) fn apply(download: Download) -> Result<()> {
    let result = apply_update(
        &download.target,
        &download.old_sha256,
        download.staged,
        |target| launch(target).spawn().map(|_| ()),
    );
    if result.is_err() {
        let _ = launch(&download.target).spawn();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_uses_the_absolute_path_with_spaces() {
        let entry = desktop_entry(Path::new("/home/usuário local/.local/bin/rustdvr")).unwrap();
        assert!(entry.contains("Exec=\"/home/usuário local/.local/bin/rustdvr\"\n"));
        assert!(entry.contains("Icon=visor-dvr\n"));
    }
}
