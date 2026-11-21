use crate::{camera_url, Brand, Equipment};
use reqwest::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use std::io::Read;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Camera {
    pub channel: u32,
    pub name: String,
}

#[derive(Clone, Debug)]
pub enum State {
    Checking,
    Connected { cameras: Option<Vec<Camera>> },
    Failed(&'static str),
    Unknown(&'static str),
}

fn parse_intelbras(body: &str) -> Vec<Camera> {
    let mut cameras = Vec::new();
    for line in body.lines() {
        let Some((field, name)) = line.split_once('=') else {
            continue;
        };
        let Some(index) = field
            .strip_prefix("table.ChannelTitle[")
            .and_then(|v| v.strip_suffix("].Name"))
            .and_then(|v| v.parse::<u32>().ok())
        else {
            continue;
        };
        if index >= 256 {
            continue;
        }
        cameras.push(Camera {
            channel: index + 1,
            name: name.trim().to_owned(),
        });
    }
    normalize(cameras)
}

fn parse_hikvision(body: &str) -> Vec<Camera> {
    let Ok(doc) = roxmltree::Document::parse(body) else {
        return vec![];
    };
    let mut cameras = Vec::new();
    for node in doc
        .descendants()
        .filter(|n| n.has_tag_name("VideoInputChannel") || n.has_tag_name("InputProxyChannel"))
    {
        let get = |key| {
            node.children()
                .find(|n| n.has_tag_name(key))
                .and_then(|n| n.text())
                .unwrap_or("")
                .trim()
        };
        let Ok(channel) = get("id").parse::<u32>() else {
            continue;
        };
        if !(1..=256).contains(&channel) {
            continue;
        }
        let name = match get("name") {
            "" => get("channelName"),
            value => value,
        };
        cameras.push(Camera {
            channel,
            name: name.to_owned(),
        });
    }
    normalize(cameras)
}

fn normalize(mut cameras: Vec<Camera>) -> Vec<Camera> {
    cameras.sort_by_key(|c| c.channel);
    cameras.dedup_by_key(|c| c.channel);
    cameras
}

pub fn label(camera: &Camera) -> String {
    if camera.name.is_empty() {
        format!("Câmera {}", camera.channel)
    } else {
        format!("{} · {}", camera.channel, camera.name)
    }
}

enum HttpResult {
    Success(Vec<Camera>, bool),
    Rejected,
    Unavailable,
}

fn http(equipment: &Equipment, password: &str, path: &str) -> HttpResult {
    let host = if equipment.host.contains(':') && !equipment.host.starts_with('[') {
        format!("[{}]", equipment.host)
    } else {
        equipment.host.clone()
    };
    let Ok(url) = url::Url::parse(&format!("http://{host}:{}{path}", equipment.http_port)) else {
        return HttpResult::Unavailable;
    };
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
    else {
        return HttpResult::Unavailable;
    };
    let Ok(mut response) = client.get(url.clone()).send() else {
        return HttpResult::Unavailable;
    };
    let mut authenticated = false;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let challenge = response
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .find(|h| h.trim_start().to_ascii_lowercase().starts_with("digest "));
        let Some(mut digest) = challenge.and_then(|s| digest_auth::parse(s).ok()) else {
            return HttpResult::Unavailable;
        };
        let uri = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        let context = digest_auth::AuthContext::new(&equipment.username, password, &uri);
        let Ok(auth) = digest.respond(&context) else {
            return HttpResult::Unavailable;
        };
        let Ok(reply) = client
            .get(url)
            .header(AUTHORIZATION, auth.to_string())
            .send()
        else {
            return HttpResult::Unavailable;
        };
        response = reply;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return HttpResult::Rejected;
        }
        authenticated = response.status().is_success();
    }
    if !response.status().is_success() {
        return HttpResult::Unavailable;
    }
    let mut body = String::new();
    if response.take(1_048_577).read_to_string(&mut body).is_err() || body.len() > 1_048_576 {
        return HttpResult::Unavailable;
    }
    let cameras = match equipment.brand {
        Brand::Intelbras => parse_intelbras(&body),
        Brand::Hikvision => parse_hikvision(&body),
        Brand::Custom | Brand::Onvif => vec![],
    };
    HttpResult::Success(cameras, authenticated)
}

pub fn discover(equipment: &Equipment, password: String) -> State {
    if equipment.brand == Brand::Onvif {
        let host = if equipment.host.contains(':') && !equipment.host.starts_with('[') {
            format!("[{}]", equipment.host)
        } else {
            equipment.host.clone()
        };
        let Ok(url) = url::Url::parse(&format!(
            "http://{host}:{}/onvif/device_service",
            equipment.http_port
        )) else {
            return State::Unknown("Endereço ONVIF inválido.");
        };
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return State::Unknown("Não foi possível iniciar a verificação ONVIF.");
        };
        let credentials = retina::client::Credentials {
            username: equipment.username.clone(),
            password,
        };
        return match runtime.block_on(crate::onvif::stream_url(&url, &credentials)) {
            Ok(_) => State::Connected {
                cameras: Some(vec![Camera {
                    channel: 1,
                    name: String::new(),
                }]),
            },
            Err(error) => State::Unknown(error),
        };
    }
    let paths: &[&str] = match equipment.brand {
        Brand::Intelbras => &["/cgi-bin/configManager.cgi?action=getConfig&name=ChannelTitle"],
        Brand::Hikvision => &[
            "/ISAPI/ContentMgmt/InputProxy/channels",
            "/ISAPI/System/Video/inputs/channels",
        ],
        Brand::Custom | Brand::Onvif => &[],
    };
    for path in paths {
        match http(equipment, &password, path) {
        HttpResult::Success(cameras, true) => return State::Connected {
            cameras: if cameras.is_empty() { None } else { Some(cameras) },
        },
        HttpResult::Success(_, false) => return State::Unknown("A consulta HTTP não exigiu autenticação; não foi possível confirmar as credenciais."),
        // Do not retry rejected credentials through another protocol.
        HttpResult::Rejected => return State::Failed("Autenticação HTTP recusada; verifique as credenciais."),
        HttpResult::Unavailable => {}
    }
    }
    let Ok(url) = camera_url(
        equipment.brand,
        &equipment.host,
        equipment.port,
        "",
        "",
        1,
        false,
    ) else {
        return State::Unknown("Não foi possível verificar este tipo de equipamento.");
    };
    let credentials = retina::client::Credentials {
        username: equipment.username.clone(),
        password,
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return State::Unknown("Não foi possível iniciar a verificação.");
    };
    match runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(8),
            retina::client::Session::describe(
                url,
                retina::client::SessionOptions::default().creds(Some(credentials)),
            ),
        )
        .await
    }) {
        Ok(Ok(_)) => State::Connected { cameras: None },
        Ok(Err(error)) if error.status_code() == Some(401) => {
            State::Failed("Autenticação RTSP recusada; verifique as credenciais.")
        }
        Ok(Err(error)) if error.status_code().is_some() => {
            State::Unknown("Equipamento respondeu, mas não foi possível verificar o canal 1.")
        }
        _ => State::Failed("Não foi possível conectar; verifique rede e portas."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn intelbras_zero_based_names_and_gaps() {
        let result = parse_intelbras(
            "table.ChannelTitle[2].Name=Fundo\nother=1\ntable.ChannelTitle[0].Name=Entrada\n",
        );
        assert_eq!(
            result,
            vec![
                Camera {
                    channel: 1,
                    name: "Entrada".into()
                },
                Camera {
                    channel: 3,
                    name: "Fundo".into()
                }
            ]
        );
    }

    #[test]
    fn hikvision_namespaces_and_noncontiguous_channels() {
        let result = parse_hikvision(
            r#"<VideoInputChannelList xmlns="urn:hik"><VideoInputChannel><id>3</id><name>Portão &amp; rua</name></VideoInputChannel><VideoInputChannel><id>1</id><name></name></VideoInputChannel></VideoInputChannelList>"#,
        );
        assert_eq!(result.len(), 2);
        assert_eq!(label(&result[0]), "Câmera 1");
        assert_eq!(label(&result[1]), "3 · Portão & rua");
    }

    #[test]
    fn malformed_data_does_not_replace_manual_channels() {
        assert!(parse_hikvision("<broken").is_empty());
        assert!(parse_intelbras("table.ChannelTitle[999].Name=X").is_empty());
    }

    #[test]
    fn discovers_hikvision_using_one_digest_response() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = vec![0; 4096];
                let n = socket.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_ascii_lowercase();
                if attempt == 0 {
                    assert!(!request.contains("authorization:"));
                    socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"test\", nonce=\"xyz\", qop=\"auth\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    assert!(request.contains("authorization: digest "));
                    let body = "<VideoInputChannelList><VideoInputChannel><id>2</id><name>Entrada</name></VideoInputChannel></VideoInputChannelList>";
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            }
        });
        let equipment = Equipment {
            id: "test".into(),
            disabled: false,
            name: "test".into(),
            brand: Brand::Hikvision,
            host: "127.0.0.1".into(),
            port: 554,
            http_port: port,
            utc_offset_minutes: None,
            username: "user".into(),
            channels: 4,
        };
        match http(&equipment, "password", "/test") {
            HttpResult::Success(cameras, true) => assert_eq!(cameras[0].channel, 2),
            _ => panic!("digest discovery failed"),
        }
        server.join().unwrap();
    }
}
