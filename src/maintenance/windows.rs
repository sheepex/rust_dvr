use super::{apply_update, copy_executable, launch, Download, Result, UPDATE_PREFIX};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
};
use windows::{
    core::{Interface, GUID, HRESULT, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE, WAIT_OBJECT_0},
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
            },
            Threading::{OpenProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_SYNCHRONIZE},
        },
        UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink,
            KF_FLAG_CREATE,
        },
    },
};

const APPLY: &str = "--rustdvr-apply-update";
const CLEAN: &str = "--rustdvr-clean-update";

pub(super) fn install_path() -> Result<PathBuf> {
    Ok(crate::config::installed_dir()
        .ok_or("Não foi possível identificar a pasta de instalação do usuário.")?
        .join("rustdvr.exe"))
}

pub(super) fn set_executable(_: &File) -> io::Result<()> {
    Ok(())
}

fn known_folder(id: &GUID) -> Result<PathBuf> {
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_CREATE, HANDLE::default())?;
        let directory = PathBuf::from(std::ffi::OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.as_ptr().cast()));
        Ok(directory)
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn write_shortcut(target: &Path, directory: &Path) -> Result<()> {
    // Use a temporary .lnk so Explorer never reads a partially written shortcut.
    let temporary = tempfile::Builder::new()
        .suffix(".lnk")
        .tempfile_in(directory)?
        .into_temp_path();
    let target_wide = wide(target.as_os_str());
    let working_dir = wide(super::parent(target)?.as_os_str());
    let temporary_wide = wide(temporary.as_os_str());
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(PCWSTR(target_wide.as_ptr()))?;
        link.SetWorkingDirectory(PCWSTR(working_dir.as_ptr()))?;
        let file: IPersistFile = link.cast()?;
        file.Save(PCWSTR(temporary_wide.as_ptr()), true)?;
    }
    temporary.persist(directory.join("RustDVR.lnk"))?;
    Ok(())
}

pub(super) fn write_shortcuts(target: &Path) -> Result<()> {
    let _com = ComApartment::initialize()?;
    let desktop = known_folder(&FOLDERID_Desktop)?;
    let programs = known_folder(&FOLDERID_Programs)?;
    write_shortcut(target, &desktop)?;
    write_shortcut(target, &programs)
}

fn wait_for_exit(pid: u32) -> Result<()> {
    unsafe {
        let process = match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(handle) => handle,
            Err(error) if error.code() == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => {
                // The process has already exited.
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let wait = WaitForSingleObject(process, 30_000);
        CloseHandle(process)?;
        if wait != WAIT_OBJECT_0 {
            return Err("O processo anterior não encerrou a tempo.".into());
        }
    }
    Ok(())
}

fn matches_helper_path(helper: &Path, target: &Path) -> bool {
    helper.is_absolute()
        && target.is_absolute()
        && helper != target
        && helper.parent() == target.parent()
        && helper
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with(UPDATE_PREFIX) && name.ends_with(".exe"))
}

pub(super) fn apply(download: Download) -> Result<()> {
    let helper = download.staged.keep()?;
    let result = launch(&helper)
        .creation_flags(CREATE_NO_WINDOW.0)
        .arg(APPLY)
        .arg(&download.target)
        .arg(std::process::id().to_string())
        .arg(download.old_sha256)
        .spawn();
    if let Err(error) = result {
        let _ = fs::remove_file(&helper);
        let _ = launch(&download.target).spawn();
        return Err(error.into());
    }
    Ok(())
}

fn execute_helper(target: &Path, pid: u32, old_sha256: &str) -> Result<()> {
    let helper = std::env::current_exe()?;
    if !matches_helper_path(&helper, target) {
        return Err("Caminho inválido para o executável de atualização.".into());
    }
    wait_for_exit(pid)?;
    let restart = |path: &Path| {
        launch(path)
            .arg(CLEAN)
            .arg(&helper)
            .arg(std::process::id().to_string())
            .spawn()
            .map(|_| ())
    };
    let result = copy_executable(&helper, target)
        .and_then(|staged| apply_update(target, old_sha256, staged, restart));
    if result.is_err() {
        let _ = restart(target);
    }
    result
}

pub(super) fn handle_internal_command() -> bool {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if let [command, target, pid, hash] = args.as_slice() {
        if command == APPLY {
            let result = pid
                .to_str()
                .and_then(|pid| pid.parse::<u32>().ok())
                .zip(hash.to_str())
                .ok_or_else(|| super::Error::from("Parâmetros de atualização inválidos."))
                .and_then(|(pid, hash)| execute_helper(Path::new(target), pid, hash));
            if let Err(error) = result {
                super::report_error("Falha ao atualizar o RustDVR", &error.to_string());
            }
            return true;
        }
    }
    if let [command, helper, pid] = args.as_slice() {
        if command == CLEAN {
            let helper = PathBuf::from(helper);
            if let (Ok(target), Some(pid)) = (
                std::env::current_exe(),
                pid.to_str().and_then(|pid| pid.parse::<u32>().ok()),
            ) {
                if matches_helper_path(&helper, &target) {
                    std::thread::spawn(move || {
                        if wait_for_exit(pid).is_ok() {
                            let _ = fs::remove_file(helper);
                        }
                    });
                }
            }
        }
    }
    false
}
