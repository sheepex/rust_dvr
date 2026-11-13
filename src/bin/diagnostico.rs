#[path = "../video.rs"]
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
    let Some(path) = directories::ProjectDirs::from("br", "visor-dvr", "visor-dvr")
        .map(|d| d.config_dir().join("equipamentos.json"))
    else {
        eprintln!("Pasta de configuração indisponível.");
        return;
    };
    let Ok(data) = std::fs::read(path) else {
        eprintln!("Não foi possível ler o cadastro.");
        return;
    };
    let Ok(items) = serde_json::from_slice::<Vec<Equipment>>(&data) else {
        eprintln!("Cadastro inválido.");
        return;
    };
    let Some(item) = items.first() else {
        eprintln!("Nenhum equipamento cadastrado.");
        return;
    };
    let Ok(entry) = keyring::Entry::new("br.visor-dvr.equipamentos", &item.id) else {
        eprintln!("Armazenamento de credenciais indisponível.");
        return;
    };
    let Ok(password) = entry.get_password() else {
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
        let result = runtime.block_on(video::run(url, creds, tx.clone(), None));
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
