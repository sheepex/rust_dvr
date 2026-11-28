#[cfg(target_os = "linux")]
#[path = "maintenance/linux.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "maintenance/windows.rs"]
mod platform;
mod release;

use eframe::egui;
use release::{Download, Release};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
};
use tempfile::{NamedTempFile, TempPath};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

const UPDATE_PREFIX: &str = ".rustdvr-update-";
#[cfg(target_os = "linux")]
const EXE_SUFFIX: &str = "";
#[cfg(target_os = "windows")]
const EXE_SUFFIX: &str = ".exe";

pub struct Restart(Download);

impl Restart {
    pub fn apply(self) -> Result<()> {
        if !release::file_sha256(&self.0.staged)
            .is_ok_and(|digest| digest.eq_ignore_ascii_case(&self.0.sha256))
        {
            let _ = launch(&self.0.target).spawn();
            return Err(
                "O arquivo verificado não está mais disponível. Baixe a atualização novamente."
                    .into(),
            );
        }
        platform::apply(self.0)
    }
}

pub fn handle_internal_command() -> bool {
    #[cfg(target_os = "windows")]
    return platform::handle_internal_command();
    #[cfg(target_os = "linux")]
    false
}

pub fn report_error(title: &str, message: &str) {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(title)
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

fn maintenance_dir() -> Result<PathBuf> {
    let path = crate::config::previous_path()
        .ok_or("A pasta de configuração do usuário está indisponível.")?;
    Ok(parent(&path)?.to_path_buf())
}

fn portable_choice() -> Result<PathBuf> {
    Ok(maintenance_dir()?.join("executar-sem-instalar"))
}

fn remember_portable_choice() -> Result<()> {
    let path = portable_choice()?;
    fs::create_dir_all(parent(&path)?)?;
    File::create(path)?;
    Ok(())
}

fn maintenance_lock() -> Result<File> {
    let directory = maintenance_dir()?;
    fs::create_dir_all(&directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("manutencao.lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn parent(path: &Path) -> Result<&Path> {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "Não foi possível identificar a pasta do executável.".into())
}

fn is_same_executable(left: &Path, right: &Path) -> bool {
    left == right || matches!((left.canonicalize(), right.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

fn install(source: &Path, target: &Path) -> Result<bool> {
    let _lock = maintenance_lock()?;
    fs::create_dir_all(parent(target)?)?;
    let use_installed_config = crate::config::copy_for_install(
        &source.with_file_name(crate::config::FILE_NAME),
        &target.with_file_name(crate::config::FILE_NAME),
    )?;
    if !target.is_file() {
        copy_executable(source, target)?.persist_noclobber(target)?;
    }
    platform::write_shortcuts(target)?;
    Ok(use_installed_config)
}

fn stage_executable(target: &Path) -> Result<NamedTempFile> {
    Ok(tempfile::Builder::new()
        .prefix(UPDATE_PREFIX)
        .suffix(EXE_SUFFIX)
        .tempfile_in(parent(target)?)?)
}

fn finish_executable(file: &File) -> io::Result<()> {
    platform::set_executable(file)?;
    file.sync_all()
}

fn copy_executable(source: &Path, target: &Path) -> Result<TempPath> {
    let mut staged = stage_executable(target)?;
    io::copy(&mut File::open(source)?, staged.as_file_mut())?;
    finish_executable(staged.as_file())?;
    Ok(staged.into_temp_path())
}

fn apply_update(
    target: &Path,
    old_sha256: &str,
    staged: TempPath,
    restart: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<()> {
    let _lock = maintenance_lock()?;
    if release::file_sha256(target)? != old_sha256 {
        return Err(
            "O executável mudou após o download. Abra o RustDVR e verifique de novo.".into(),
        );
    }
    let backup = copy_executable(target, target)?;
    if platform::install_path().is_ok_and(|path| is_same_executable(&path, target)) {
        platform::write_shortcuts(target)?;
    }
    replace_and_restart(target, staged, backup, restart)
}

fn replace_and_restart(
    target: &Path,
    staged: TempPath,
    backup: TempPath,
    restart: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<()> {
    staged.persist(target)?;
    if let Err(error) = restart(target) {
        backup.persist(target).map_err(|rollback| {
            format!("Falha ao abrir a nova versão ({error}) e restaurar a anterior ({rollback}).")
        })?;
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_the_previous_executable_when_restart_fails() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("rustdvr");
        let updated = directory.path().join("update");
        fs::write(&target, b"old").unwrap();
        fs::write(&updated, b"new").unwrap();
        let backup = copy_executable(&target, &target).unwrap();
        let staged = copy_executable(&updated, &target).unwrap();

        let result = replace_and_restart(&target, staged, backup, |path| {
            assert_eq!(fs::read(path).unwrap(), b"new");
            Err(io::Error::other("could not start"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }
}

fn launch(target: &Path) -> Command {
    let mut command = Command::new(target);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub struct Maintenance {
    executable: Option<PathBuf>,
    moved_config: Option<PathBuf>,
    install_path: Option<PathBuf>,
    installed: bool,
    prompt_install: bool,
    check: Option<(Receiver<Result<Option<Release>>>, bool)>,
    release: Option<Release>,
    work: Option<Receiver<Result<Completed>>>,
    ready: Option<Download>,
    prompt_restart: bool,
    notice: Option<String>,
    restart: Sender<Restart>,
}

enum Completed {
    Installed(PathBuf, bool),
    Shortcuts,
    Downloaded(Download),
}

fn background<T: Send + 'static>(
    ctx: &egui::Context,
    run: impl FnOnce() -> T + Send + 'static,
) -> Receiver<T> {
    let ctx = ctx.clone();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let _ = send.send(run());
        ctx.request_repaint();
    });
    receive
}

fn poll<T>(job: &mut Option<Receiver<Result<T>>>) -> Option<Result<T>> {
    let result = match job.as_ref()?.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Err("A operação terminou inesperadamente.".into()),
    };
    *job = None;
    Some(result)
}

impl Maintenance {
    pub fn new(ctx: &egui::Context, restart: Sender<Restart>) -> Self {
        let executable = std::env::current_exe().ok();
        let install_path = platform::install_path().ok();
        let installed = install_path.as_ref().is_some_and(|path| path.is_file());
        let declined = portable_choice().is_ok_and(|path| path.is_file());
        let mut this = Self {
            prompt_install: executable.is_some()
                && install_path.is_some()
                && !installed
                && !declined,
            executable,
            moved_config: None,
            install_path,
            installed,
            check: None,
            release: None,
            work: None,
            ready: None,
            prompt_restart: false,
            notice: None,
            restart,
        };
        this.check_for_updates(ctx, false);
        this
    }

    pub fn moved_config(&self) -> Option<&Path> {
        self.moved_config.as_deref()
    }

    fn check_for_updates(&mut self, ctx: &egui::Context, manual: bool) {
        if release::supported() && self.executable.is_some() {
            self.check = Some((background(ctx, release::latest), manual));
        }
    }

    fn poll(&mut self) {
        if let Some((receive, manual)) = &self.check {
            let result = match receive.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Disconnected) => {
                    Err("A consulta terminou inesperadamente.".into())
                }
                Err(TryRecvError::Empty) => return self.poll_work(),
            };
            let manual = *manual;
            self.check = None;
            match result {
                Ok(Some(release)) => self.release = Some(release),
                Ok(None) if manual => self.notice = Some("O RustDVR está atualizado.".into()),
                Err(error) if manual => {
                    self.notice = Some(format!("Falha ao consultar o GitHub. {error}"));
                }
                _ => {}
            }
        }
        self.poll_work();
    }

    fn poll_work(&mut self) {
        match poll(&mut self.work) {
            Some(Ok(Completed::Installed(path, use_installed_config))) => {
                self.installed = true;
                if use_installed_config {
                    self.moved_config = Some(path.with_file_name(crate::config::FILE_NAME));
                }
                self.executable = Some(path);
                self.notice = Some(if use_installed_config {
                    "O RustDVR foi instalado e os atalhos foram criados.".into()
                } else {
                    "O RustDVR foi instalado. Abra pelo atalho para usar as configurações que já estavam na pasta de instalação.".into()
                });
            }
            Some(Ok(Completed::Shortcuts)) => self.notice = Some("Atalhos atualizados.".into()),
            Some(Ok(Completed::Downloaded(download))) => {
                self.ready = Some(download);
                self.prompt_restart = true;
            }
            Some(Err(error)) => {
                self.installed = self
                    .install_path
                    .as_ref()
                    .is_some_and(|path| path.is_file());
                self.notice = Some(format!("Não foi possível concluir a operação. {error}"));
            }
            None => {}
        }
    }

    pub fn about(&mut self, ui: &mut egui::Ui) {
        let idle = self.work.is_none();
        if let Some(path) = self.install_path.clone() {
            if self.installed {
                if ui
                    .add_enabled(idle, egui::Button::new("Recriar atalhos"))
                    .clicked()
                {
                    self.work = Some(background(ui.ctx(), move || {
                        let _lock = maintenance_lock()?;
                        platform::write_shortcuts(&path)?;
                        Ok(Completed::Shortcuts)
                    }));
                }
            } else if ui
                .add_enabled(
                    idle && self.executable.is_some(),
                    egui::Button::new("Instalar neste computador"),
                )
                .clicked()
            {
                self.prompt_install = true;
            }
        }
        if self.ready.is_some() {
            if ui.button("Concluir atualização").clicked() {
                self.prompt_restart = true;
            }
        } else if release::supported()
            && self.executable.is_some()
            && ui
                .add_enabled(
                    idle && self.check.is_none() && self.release.is_none(),
                    egui::Button::new("Verificar atualizações"),
                )
                .clicked()
        {
            self.check_for_updates(ui.ctx(), true);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, can_restart: bool) {
        self.poll();
        if self.prompt_install && self.work.is_none() && self.ready.is_none() {
            let mut accepted = false;
            let mut portable = false;
            egui::Modal::new(egui::Id::new("rustdvr-install")).show(ctx, |ui| {
                ui.heading("Instalar o RustDVR?");
                ui.label("A instalação fica na sua conta de usuário e cria atalhos para abrir o programa.");
                if let Some(path) = &self.install_path {
                    ui.label(path.display().to_string());
                }
                ui.horizontal(|ui| {
                    accepted = ui.button("Instalar").clicked();
                    portable = ui.button("Apenas executar").clicked();
                });
            });
            if accepted {
                self.prompt_install = false;
                if let (Some(source), Some(target)) =
                    (self.executable.clone(), self.install_path.clone())
                {
                    self.work = Some(background(ctx, move || {
                        let use_installed_config = install(&source, &target)?;
                        Ok(Completed::Installed(target, use_installed_config))
                    }));
                }
            } else if portable {
                self.prompt_install = false;
                if let Err(error) = remember_portable_choice() {
                    self.notice = Some(format!("Não foi possível guardar a preferência. {error}"));
                }
            }
            return;
        }

        if self.work.is_some() {
            egui::Modal::new(egui::Id::new("rustdvr-maintenance-work")).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Processando. Aguarde…");
                });
            });
            return;
        }
        if let Some(message) = &self.notice {
            let response =
                egui::Modal::new(egui::Id::new("rustdvr-maintenance-notice")).show(ctx, |ui| {
                    ui.label(message);
                    ui.button("OK").clicked()
                });
            if response.inner || response.should_close() {
                self.notice = None;
            }
            return;
        }
        if self.prompt_restart {
            self.show_restart(ctx, can_restart);
        } else if let Some(release) = &self.release {
            let mut download = false;
            let mut later = false;
            egui::Modal::new(egui::Id::new("rustdvr-update")).show(ctx, |ui| {
                ui.heading("Atualização disponível");
                ui.label(format!(
                    "A versão {} do RustDVR está disponível no GitHub.",
                    release.version
                ));
                ui.horizontal(|ui| {
                    download = ui.button("Baixar atualização").clicked();
                    later = ui.button("Agora não").clicked();
                });
            });
            if download {
                if let (Some(release), Some(target)) =
                    (self.release.take(), self.executable.clone())
                {
                    self.work = Some(background(ctx, move || {
                        Ok(Completed::Downloaded(release.download(target)?))
                    }));
                }
            } else if later {
                self.release = None;
            }
        }
    }

    fn show_restart(&mut self, ctx: &egui::Context, can_restart: bool) {
        let Some(ready) = &self.ready else {
            return;
        };
        let mut restart = false;
        let mut later = false;
        egui::Modal::new(egui::Id::new("rustdvr-update-restart")).show(ctx, |ui| {
            ui.heading("Atualização baixada");
            ui.label(format!(
                "Fechar e reabrir o RustDVR para usar a versão {}?",
                ready.version
            ));
            if !can_restart {
                ui.label("Aguarde a exportação de vídeo terminar para reiniciar.");
            }
            ui.horizontal(|ui| {
                restart = ui
                    .add_enabled(can_restart, egui::Button::new("Fechar e reabrir"))
                    .clicked();
                later = ui.button("Mais tarde").clicked();
            });
        });
        if restart {
            let Some(download) = self.ready.take() else {
                return;
            };
            self.prompt_restart = false;
            if self.restart.send(Restart(download)).is_ok() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        } else if later {
            self.prompt_restart = false;
        }
    }
}
