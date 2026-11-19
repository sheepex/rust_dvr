use retina::client::Credentials;
use url::Url;

const FAILURE: &str = "Falha ONVIF: confira rede, usuário, senha e serviços locais da câmera.";

fn host_matches(base: &Url, candidate: &Url) -> bool {
    match (base.host_str(), candidate.host_str()) {
        (Some(a), Some(b)) => {
            match (a.parse::<std::net::IpAddr>(), b.parse::<std::net::IpAddr>()) {
                (Ok(a), Ok(b)) => a == b,
                (Err(_), Err(_)) => a.eq_ignore_ascii_case(b),
                _ => false,
            }
        }
        _ => false,
    }
}

fn same_host(base: &Url, candidate: &Url) -> bool {
    host_matches(base, candidate)
        && candidate.username().is_empty()
        && candidate.password().is_none()
}

pub async fn replay_url(device: &Url, credentials: &Credentials) -> Result<Url, &'static str> {
    let client = oxvif::OnvifClient::new(device.as_str())
        .with_credentials(&credentials.username, &credentials.password);
    let client = match client.get_system_date_and_time().await {
        Ok(time) => client.with_utc_offset(time.utc_offset_secs()),
        Err(_) => client,
    };
    let caps = client.get_capabilities().await.map_err(|_| FAILURE)?;
    let recording = caps
        .recording
        .url
        .as_deref()
        .ok_or("Equipamento não oferece gravações ONVIF.")?;
    let replay = caps
        .replay
        .url
        .as_deref()
        .ok_or("Equipamento não oferece reprodução ONVIF.")?;
    for endpoint in [recording, replay] {
        let parsed = Url::parse(endpoint).map_err(|_| FAILURE)?;
        if !matches!(parsed.scheme(), "http" | "https") || !same_host(device, &parsed) {
            return Err("Endereço do serviço ONVIF não corresponde à câmera cadastrada.");
        }
    }
    let items = client
        .get_recordings(recording)
        .await
        .map_err(|_| "Não foi possível consultar gravações ONVIF.")?;
    if items.len() != 1 {
        return Err("Reprodução ONVIF exige exatamente uma gravação; seleção de canais ainda não disponível.");
    }
    let uri = client
        .get_replay_uri(replay, &items[0].token, "RTP-Unicast", "RTSP")
        .await
        .map_err(|_| "Não foi possível obter endereço de reprodução ONVIF.")?;
    let mut url = Url::parse(&uri).map_err(|_| FAILURE)?;
    if url.scheme() != "rtsp" || !host_matches(device, &url) {
        return Err("Endereço de reprodução ONVIF não corresponde à câmera cadastrada.");
    }
    url.set_username("").map_err(|_| FAILURE)?;
    url.set_password(None).map_err(|_| FAILURE)?;
    Ok(url)
}

pub async fn stream_url(device: &Url, credentials: &Credentials) -> Result<Url, &'static str> {
    let client = oxvif::OnvifClient::new(device.as_str())
        .with_credentials(&credentials.username, &credentials.password);
    #[cfg(test)]
    let client = if std::env::var_os("ONVIF_TEST_DIGEST_ONLY").is_some() {
        client.with_authentication_mode(oxvif::client::AuthenticationMode::Digest)
    } else {
        client
    };
    let client = match client.get_system_date_and_time().await {
        Ok(time) => client.with_utc_offset(time.utc_offset_secs()),
        Err(_) => client,
    };
    let caps = client
        .get_capabilities()
        .await
        .map_err(|_| "Falha ao consultar capacidades ONVIF; confira rede e credenciais.")?;
    let media = caps.media.url.as_deref().ok_or(FAILURE)?;
    let media_url = Url::parse(media).map_err(|_| FAILURE)?;
    if !matches!(media_url.scheme(), "http" | "https") || !same_host(device, &media_url) {
        return Err("Endereço do serviço ONVIF não corresponde à câmera cadastrada.");
    }
    let profiles = client
        .get_profiles(media)
        .await
        .map_err(|error| match error {
            oxvif::OnvifError::Transport(oxvif::transport::TransportError::HttpStatus {
                status: 401,
                ..
            }) => "Autenticação ONVIF recusada; confira usuário e senha de acesso local.",
            oxvif::OnvifError::Soap(oxvif::soap::SoapError::InvalidValue { field, .. })
                if field == "Encoding" =>
            {
                "A câmera informou um codec ONVIF não reconhecido pela biblioteca."
            }
            oxvif::OnvifError::Soap(oxvif::soap::SoapError::MissingField(_)) => {
                "A resposta ONVIF dos perfis não contém um campo obrigatório."
            }
            oxvif::OnvifError::Soap(error) if error.not_authorized() => {
                "Autenticação ONVIF recusada; confira usuário e senha de acesso local."
            }
            oxvif::OnvifError::Soap(oxvif::soap::SoapError::Fault { .. }) => {
                "A câmera recusou a consulta ONVIF de perfis."
            }
            oxvif::OnvifError::Soap(_) => "A resposta ONVIF dos perfis não pôde ser interpretada.",
            oxvif::OnvifError::Transport(_) => {
                "Falha de comunicação ou autenticação ao consultar os perfis ONVIF."
            }
            _ => "Falha ao consultar perfis ONVIF; confira usuário, senha e serviços locais.",
        })?;
    let profile = profiles
        .first()
        .ok_or("A câmera não informou perfis ONVIF de vídeo.")?;
    let uri = client
        .get_stream_uri(media, &profile.token)
        .await
        .map_err(|_| "Falha ao obter endereço RTSP pelo ONVIF.")?;
    let mut url =
        Url::parse(&uri.uri).map_err(|_| "A câmera informou uma URL de vídeo inválida.")?;
    // Never forward credentials to a different host, even if advertised by a camera.
    if url.scheme() != "rtsp" || !host_matches(device, &url) {
        return Err("Endereço do vídeo ONVIF não corresponde à câmera cadastrada.");
    }
    url.set_username("").map_err(|_| FAILURE)?;
    url.set_password(None).map_err(|_| FAILURE)?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requer câmera cadastrada e acessível"]
    fn real_camera_clock() {
        let name = std::env::var("ONVIF_TEST_NAME").unwrap();
        let (config, _) =
            crate::config::load_local::<crate::Equipment>(&crate::config::path().unwrap()).unwrap();
        let item = config
            .equipment
            .iter()
            .find(|item| item.name == name)
            .unwrap();
        let password = config
            .passwords
            .as_ref()
            .and_then(|p| p.get(&item.id))
            .unwrap();
        let device = format!(
            "http://{}:{}/onvif/device_service",
            item.host, item.http_port
        );
        let client =
            oxvif::OnvifClient::new(&device).with_credentials(&item.username, password.as_str());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            match tokio::time::timeout(std::time::Duration::from_secs(8), client.get_system_date_and_time()).await {
                Ok(Ok(time)) => println!("Relógio UTC em relação ao computador: {} segundos; fuso informado: {:?}; horário de verão: {}.", time.utc_offset_secs(), time.timezone, time.daylight_savings),
                _ => println!("Não foi possível consultar o relógio ONVIF."),
            }
        });
    }

    #[test]
    #[ignore = "requer uma câmera e intervalo autorizados"]
    fn real_camera_replay_from_local_config() {
        let name = std::env::var("ONVIF_TEST_NAME").expect("defina ONVIF_TEST_NAME");
        let start: jiff::civil::DateTime = std::env::var("ONVIF_TEST_UTC_START")
            .expect("defina ONVIF_TEST_UTC_START")
            .parse()
            .expect("data UTC inválida");
        let path = crate::config::path().unwrap();
        let (config, _) = crate::config::load_local::<crate::Equipment>(&path).unwrap();
        let item = config
            .equipment
            .iter()
            .find(|item| item.name == name)
            .expect("equipamento não encontrado");
        let password = config
            .passwords
            .as_ref()
            .and_then(|p| p.get(&item.id))
            .expect("senha não cadastrada");
        let device = Url::parse(&format!(
            "http://{}:{}/onvif/device_service",
            item.host, item.http_port
        ))
        .unwrap();
        let credentials = Credentials {
            username: item.username.clone(),
            password: password.to_string(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stream = runtime
            .block_on(replay_url(&device, &credentials))
            .expect("consulta de reprodução ONVIF falhou");
        let end = start + jiff::SignedDuration::from_secs(60);
        let options = crate::video::PlaybackOptions {
            rate: 1.0,
            sessions: Default::default(),
            requested_utc: Some(start),
            buffer: Default::default(),
            prefill_seconds: 1.0,
            onvif_range: Some(format!(
                "clock={}-{}",
                start.strftime("%Y%m%dT%H%M%SZ"),
                end.strftime("%Y%m%dT%H%M%SZ")
            )),
        };
        let buffer = options.buffer.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(crate::video::run(
                stream,
                credentials,
                tx.clone(),
                None,
                Some(options),
                true,
            ));
            if let Err(message) = result {
                let _ = tx.try_send(crate::video::Event::Status(message));
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let (Some(frame), _) = buffer.take_due(f64::INFINITY) {
                println!(
                    "Quadro de gravação decodificado: {} × {}.",
                    frame.width, frame.height
                );
                break;
            }
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(crate::video::Event::Status(message)) => {
                    if message.starts_with("A câmera não confirmou o horário") {
                        continue;
                    }
                    if let (Some(frame), _) = buffer.take_due(f64::INFINITY) {
                        println!(
                            "Quadro de gravação decodificado: {} × {}.",
                            frame.width, frame.height
                        );
                        break;
                    }
                    panic!("Falha na reprodução: {message}");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if std::time::Instant::now() < deadline => {}
                _ => panic!("Tempo limite ou fim da gravação sem quadro."),
            }
        }
    }

    #[test]
    #[ignore = "requer uma câmera cadastrada e acessível"]
    fn real_camera_recording_capabilities() {
        let name = std::env::var("ONVIF_TEST_NAME").expect("defina ONVIF_TEST_NAME");
        let path = crate::config::path().unwrap();
        let (config, _) = crate::config::load_local::<crate::Equipment>(&path).unwrap();
        let item = config
            .equipment
            .iter()
            .find(|item| item.name == name)
            .expect("equipamento não encontrado");
        let password = config
            .passwords
            .as_ref()
            .and_then(|p| p.get(&item.id))
            .expect("senha não cadastrada");
        let device = Url::parse(&format!(
            "http://{}:{}/onvif/device_service",
            item.host, item.http_port
        ))
        .unwrap();
        let client = oxvif::OnvifClient::new(device.as_str())
            .with_credentials(&item.username, password.as_str());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = match client.get_system_date_and_time().await {
                Ok(time) => client.with_utc_offset(time.utc_offset_secs()),
                Err(_) => client,
            };
            match tokio::time::timeout(
                std::time::Duration::from_secs(12),
                client.get_capabilities(),
            )
            .await
            {
                Ok(Ok(caps)) => {
                    println!(
                        "Serviços anunciados: gravação={}, busca={}, reprodução={}",
                        caps.recording.url.is_some(),
                        caps.search.url.is_some(),
                        caps.replay.url.is_some()
                    );
                    if let Some(search) = caps.search.url.as_deref() {
                        if Url::parse(search).ok().is_some_and(|url| matches!(url.scheme(), "http" | "https") && same_host(&device, &url)) {
                            if let Ok(Ok(token)) = tokio::time::timeout(std::time::Duration::from_secs(10), client.find_recordings(search, Some(10), "PT15S")).await {
                                let result = tokio::time::timeout(std::time::Duration::from_secs(10), client.get_recording_search_results(search, &token, 10, "PT5S")).await;
                                if let Ok(Ok(found)) = result {
                                    for info in found.recording_information {
                                        let from = info.earliest_recording.as_deref().and_then(|s| s.parse::<jiff::Timestamp>().ok());
                                        let to = info.latest_recording.as_deref().and_then(|s| s.parse::<jiff::Timestamp>().ok());
                                        let target = std::env::var("ONVIF_TEST_UTC_START").ok()
                                            .and_then(|s| format!("{s}Z").parse::<jiff::Timestamp>().ok());
                                        println!("Limites disponíveis: {}.", from.is_some() && to.is_some());
                                        if let (Some((a, b)), Some(target)) = (from.zip(to), target) {
                                            println!("Horário solicitado dentro dos limites: {}.", a <= target && target <= b);
                                        }
                                    }
                                } else {
                                    println!("A busca não forneceu limites de gravação.");
                                }
                                let _ = client.end_search(search, &token).await;
                            } else {
                                println!("A busca de gravações não pôde ser iniciada.");
                            }
                        }
                    }
                    if let Some(recording) = caps.recording.url.as_deref() {
                        if Url::parse(recording).ok().is_some_and(|url| {
                            matches!(url.scheme(), "http" | "https") && same_host(&device, &url)
                        }) {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(12),
                                client.get_recordings(recording),
                            )
                            .await
                            {
                                Ok(Ok(items)) => {
                                    println!("Consulta de gravações aceita; {} entrada(s).", items.len());
                                    if let (Some(item), Some(replay)) = (items.first(), caps.replay.url.as_deref()) {
                                        if Url::parse(replay).ok().is_some_and(|url| matches!(url.scheme(), "http" | "https") && same_host(&device, &url)) {
                                            match tokio::time::timeout(std::time::Duration::from_secs(12), client.get_replay_uri(replay, &item.token, "RTP-Unicast", "RTSP")).await {
                                                Ok(Ok(uri)) => println!("Endereço de reprodução fornecido; mesmo host: {}.", Url::parse(&uri).ok().is_some_and(|url| url.scheme() == "rtsp" && host_matches(&device, &url))),
                                                Ok(Err(_)) => println!("Consulta de endereço de reprodução recusada ou não suportada."),
                                                Err(_) => println!("Tempo limite na consulta de endereço de reprodução."),
                                            }
                                        }
                                    }
                                },
                                Ok(Err(oxvif::OnvifError::Transport(
                                    oxvif::transport::TransportError::HttpStatus { status, .. },
                                ))) => println!("Consulta de gravações recusada: HTTP {status}."),
                                Ok(Err(oxvif::OnvifError::Soap(error)))
                                    if error.not_authorized() =>
                                {
                                    println!("Consulta de gravações: autenticação recusada.")
                                }
                                Ok(Err(_)) => {
                                    println!("Consulta de gravações falhou ou não é suportada.")
                                }
                                Err(_) => println!("Tempo limite na consulta de gravações."),
                            }
                        } else {
                            println!("Endereço de gravações rejeitado por segurança.");
                        }
                    }
                }
                Ok(Err(_)) => println!("Não foi possível consultar as capacidades ONVIF."),
                Err(_) => println!("Tempo limite ao consultar capacidades ONVIF."),
            }
        });
    }

    #[test]
    fn rejects_other_hosts_and_embedded_credentials() {
        let base = Url::parse("http://192.0.2.1/onvif/device_service").unwrap();
        assert!(same_host(
            &base,
            &Url::parse("http://192.0.2.1/onvif/Media").unwrap()
        ));
        assert!(!same_host(
            &base,
            &Url::parse("http://192.0.2.2/onvif/Media").unwrap()
        ));
        assert!(!same_host(
            &base,
            &Url::parse("http://user@192.0.2.1/onvif/Media").unwrap()
        ));
    }

    #[test]
    fn compares_rtsp_and_http_hosts_without_cross_host_redirects() {
        let base = Url::parse("http://192.0.2.1/onvif/device_service").unwrap();
        assert!(host_matches(
            &base,
            &Url::parse("rtsp://192.0.2.1:554/live").unwrap()
        ));
        assert!(!host_matches(
            &base,
            &Url::parse("rtsp://192.0.2.2:554/live").unwrap()
        ));
        let base = Url::parse("http://camera.local/onvif").unwrap();
        assert!(host_matches(
            &base,
            &Url::parse("rtsp://CAMERA.local/live").unwrap()
        ));
        assert!(!host_matches(
            &base,
            &Url::parse("rtsp://other.local/live").unwrap()
        ));
    }

    #[test]
    #[ignore = "requer uma câmera cadastrada e acessível"]
    fn real_camera_from_local_config() {
        let host = std::env::var("ONVIF_TEST_HOST").expect("defina ONVIF_TEST_HOST");
        let path = crate::config::path().unwrap();
        let (config, _) = crate::config::load_local::<crate::Equipment>(&path).unwrap();
        let equipment = config.equipment.iter().find(|item| item.host == host);
        let password = std::env::var("ONVIF_TEST_PASSWORD")
            .ok()
            .or_else(|| {
                equipment
                    .and_then(|equipment| {
                        config
                            .passwords
                            .as_ref()
                            .and_then(|values| values.get(&equipment.id))
                    })
                    .map(|value| value.to_string())
            })
            .expect("senha não cadastrada");
        let device = Url::parse(&format!(
            "http://{}:{}/onvif/device_service",
            host,
            equipment.map_or(80, |item| item.http_port)
        ))
        .unwrap();
        let credentials = Credentials {
            username: equipment.map_or_else(|| "admin".into(), |item| item.username.clone()),
            password,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stream = runtime.block_on(async {
            let stream = stream_url(&device, &credentials)
                .await
                .expect("falha ao consultar ONVIF");
            println!(
                "ONVIF autenticado; porta RTSP anunciada: {}",
                stream.port().unwrap_or(554)
            );
            let result = retina::client::Session::describe(
                stream.clone(),
                retina::client::SessionOptions::default().creds(Some(credentials.clone())),
            )
            .await;
            assert!(
                result.is_ok(),
                "não foi possível abrir o fluxo RTSP anunciado"
            );
            stream
        });
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(crate::video::run(
                stream,
                credentials,
                tx.clone(),
                None,
                None,
                true,
            ));
            if let Err(message) = result {
                let _ = tx.try_send(crate::video::Event::Status(message));
            }
        });
        match rx.recv_timeout(std::time::Duration::from_secs(25)) {
            Ok(crate::video::Event::Frame(frame)) => {
                println!("Quadro decodificado: {} × {}.", frame.width, frame.height)
            }
            Ok(crate::video::Event::Status(message)) => {
                panic!("Falha ao decodificar vídeo: {message}")
            }
            Err(_) => panic!("Tempo limite para receber vídeo."),
        }
    }
}
