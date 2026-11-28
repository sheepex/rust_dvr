#[path = "../config.rs"]
#[allow(dead_code)]
mod config;
#[path = "../video.rs"]
#[allow(dead_code)]
mod video;
use retina::client::Credentials;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
struct Equipment {
    id: String,
    host: String,
    port: u16,
    username: String,
}

fn main() {
    let Some(path) = config::path() else {
        eprintln!("Pasta de configuração indisponível.");
        return;
    };
    let mut fallbacks = Vec::new();
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fallbacks.extend(config::installed_dir().map(|dir| dir.join(config::FILE_NAME)));
    fallbacks.extend(config::previous_path());
    let Ok((config, _)) = config::load_local_with_fallbacks::<Equipment>(&path, &fallbacks) else {
        eprintln!("Cadastro inválido.");
        return;
    };
    let Some(item) = config.equipment.first() else {
        eprintln!("Nenhum equipamento cadastrado.");
        return;
    };
    let password = if let Some(passwords) = &config.passwords {
        passwords
            .get(&item.id)
            .map(|password| password.as_str().to_owned())
    } else {
        config::legacy_password(&item.id).ok().flatten()
    };
    let Some(password) = password else {
        eprintln!("Não foi possível obter a senha salva no gerenciador de credenciais.");
        return;
    };
    let Ok(url) = url::Url::parse(&format!(
        "rtsp://{}:{}/cam/realmonitor?channel=1&subtype=1",
        item.host, item.port
    )) else {
        eprintln!("Endereço inválido.");
        return;
    };
    let creds = Credentials {
        username: item.username.clone(),
        password,
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(video::run(url, creds, tx.clone(), None, None, false));
        if let Err(message) = result {
            let _ = tx.send(video::Event::Status(message));
        }
    });
    match rx.recv_timeout(Duration::from_secs(25)) {
        Ok(video::Event::Frame(frame)) => println!(
            "Quadro RGB decodificado pelo motor do aplicativo: {} × {}, {} bytes.",
            frame.width,
            frame.height,
            frame.rgb.len()
        ),
        Ok(video::Event::Status(message)) => println!("{message}"),
        Err(_) => println!("Tempo limite ao conectar ou decodificar."),
    }
}
