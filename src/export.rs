//! Progressive recording export. Matroska accepts decode-order blocks with
//! presentation timestamps and does not require buffering the whole video.
use futures::StreamExt;
use retina::client::{
    Credentials, InitialSequenceNumberPolicy, PacketItem, PlayOptions, Session, SessionOptions,
    SetupOptions,
};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

#[path = "hik.rs"]
mod hik;

fn size(mut n: u64) -> Vec<u8> {
    for len in 1..=8 {
        if n < (1u64 << (7 * len)) - 1 {
            n |= 1u64 << (7 * len);
            return n.to_be_bytes()[8 - len..].to_vec();
        }
    }
    unreachable!()
}

fn element(id: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(id.len() + 8 + data.len());
    out.extend_from_slice(id);
    out.extend_from_slice(&size(data.len() as u64));
    out.extend_from_slice(data);
    out
}
fn uint(id: &[u8], n: u64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    element(id, &bytes[start..])
}
fn master(id: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    element(id, &children.concat())
}

struct Writer<W: Write> {
    out: W,
    cluster: Option<i64>,
    frames: u64,
}

impl<W: Write> Writer<W> {
    fn new(
        mut out: W,
        codec: &str,
        extra: &[u8],
        width: u32,
        height: u32,
    ) -> std::io::Result<Self> {
        let header = master(
            &[0x1a, 0x45, 0xdf, 0xa3],
            &[
                uint(&[0x42, 0x86], 1),
                uint(&[0x42, 0xf7], 1),
                uint(&[0x42, 0xf2], 4),
                uint(&[0x42, 0xf3], 8),
                element(&[0x42, 0x82], b"matroska"),
                uint(&[0x42, 0x87], 4),
                uint(&[0x42, 0x85], 2),
            ],
        );
        out.write_all(&header)?;
        // Unknown-size segment and clusters permit immediate writes without seeking.
        out.write_all(&[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ])?;
        let info = master(
            &[0x15, 0x49, 0xa9, 0x66],
            &[
                uint(&[0x2a, 0xd7, 0xb1], 1_000_000),
                element(&[0x4d, 0x80], b"RustDVR"),
                element(&[0x57, 0x41], b"RustDVR"),
            ],
        );
        out.write_all(&info)?;
        let video = master(
            &[0xe0],
            &[uint(&[0xb0], width.into()), uint(&[0xba], height.into())],
        );
        let track = master(
            &[0xae],
            &[
                uint(&[0xd7], 1),
                uint(&[0x73, 0xc5], 1),
                uint(&[0x83], 1),
                element(&[0x86], codec.as_bytes()),
                element(&[0x63, 0xa2], extra),
                video,
            ],
        );
        out.write_all(&master(&[0x16, 0x54, 0xae, 0x6b], &[track]))?;
        Ok(Self {
            out,
            cluster: None,
            frames: 0,
        })
    }

    fn frame(&mut self, time_ms: i64, data: &[u8], key: bool) -> std::io::Result<()> {
        if time_ms < 0 {
            return Err(std::io::Error::other("timestamp negativo"));
        }
        let new_cluster = self
            .cluster
            .is_none_or(|base| time_ms - base > 30_000 || time_ms < base);
        if new_cluster {
            self.out.write_all(&[
                0x1f, 0x43, 0xb6, 0x75, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ])?;
            self.out.write_all(&uint(&[0xe7], time_ms as u64))?;
            self.cluster = Some(time_ms);
        }
        let offset = (time_ms - self.cluster.unwrap()) as i16;
        let mut prefix = vec![0xa3];
        prefix.extend_from_slice(&size((data.len() + 4) as u64));
        prefix.push(0x81);
        prefix.extend_from_slice(&offset.to_be_bytes());
        prefix.push(if key { 0x80 } else { 0 });
        self.out.write_all(&prefix)?;
        self.out.write_all(data)?;
        self.frames += 1;
        Ok(())
    }
}

fn append_nal(frame: &mut Vec<u8>, nal: &[u8]) -> Result<(), &'static str> {
    if nal.is_empty() || frame.len().saturating_add(nal.len()) > 32 * 1024 * 1024 {
        return Err("Quadro inválido ou grande demais para exportar.");
    }
    let len = u32::try_from(nal.len()).map_err(|_| "Quadro grande demais.")?;
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(nal);
    Ok(())
}

/// The output must not already exist. An interrupted file is kept for recovery.
pub async fn run(
    url: url::Url,
    creds: Credentials,
    path: &Path,
) -> Result<&'static str, &'static str> {
    let hik = url
        .path()
        .to_ascii_lowercase()
        .starts_with("/streaming/tracks/");
    let key = creds.password.clone();
    let mut session = tokio::time::timeout(
        Duration::from_secs(12),
        Session::describe(url, SessionOptions::default().creds(Some(creds))),
    )
    .await
    .map_err(|_| "Tempo esgotado ao conectar para exportação.")?
    .map_err(|_| "Falha ao conectar para exportação; confira acesso e intervalo.")?;
    let index = session
        .streams()
        .iter()
        .position(|s| s.media() == "video")
        .ok_or("O equipamento não anunciou vídeo.")?;
    let codec = session.streams()[index]
        .encoding_name()
        .to_ascii_lowercase();
    if codec != "h264" && codec != "h265" {
        return Err("Codec de exportação não suportado.");
    }
    let (extra, width, height) = match session.streams()[index].parameters() {
        Some(retina::codec::ParametersRef::Video(p)) => {
            let (w, h) = p.pixel_dimensions();
            (p.extra_data().to_vec(), w, h)
        }
        _ => return Err("Equipamento não forneceu parâmetros de vídeo para exportar."),
    };
    if extra.is_empty() {
        return Err("Parâmetros de vídeo indisponíveis para exportar.");
    }
    tokio::time::timeout(
        Duration::from_secs(12),
        session.setup(index, SetupOptions::default()),
    )
    .await
    .map_err(|_| "Tempo esgotado ao preparar exportação.")?
    .map_err(|_| "Falha ao preparar exportação.")?;
    let options = if hik {
        PlayOptions::default().initial_seq(InitialSequenceNumberPolicy::Ignore)
    } else {
        PlayOptions::default()
    };
    let session = tokio::time::timeout(Duration::from_secs(12), session.play(options))
        .await
        .map_err(|_| "Tempo esgotado ao iniciar exportação.")?
        .map_err(|_| "Equipamento recusou a exportação.")?;
    let mut file_options = std::fs::OpenOptions::new();
    file_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file_options.mode(0o600);
    }
    let file = file_options
        .open(path)
        .map_err(|_| "Não foi possível criar o arquivo; escolha um nome que ainda não exista.")?;
    let mut writer = Writer::new(
        BufWriter::new(file),
        if codec == "h265" {
            "V_MPEGH/ISO/HEVC"
        } else {
            "V_MPEG4/ISO/AVC"
        },
        &extra,
        width,
        height,
    )
    .map_err(|_| "Não foi possível gravar o cabeçalho do arquivo.")?;
    if hik && codec == "h265" {
        export_hik(session, index, &key, &mut writer).await?;
    } else {
        let mut stream = session
            .demuxed()
            .map_err(|_| "Formato RTSP incompatível para exportação.")?;
        let mut origin = None;
        let mut started = false;
        let mut deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let next = tokio::time::timeout_at(deadline, stream.next())
                .await
                .map_err(|_| {
                    "Exportação interrompida por tempo esgotado; arquivo pode estar incompleto."
                })?;
            let Some(item) = next else { break };
            let item = item.map_err(|_| "Erro no fluxo; arquivo pode estar incompleto.")?;
            if let retina::codec::CodecItem::VideoFrame(frame) = item {
                if frame.stream_id() != index {
                    continue;
                }
                deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                if frame.loss() > 0 {
                    return Err(
                        "Perda de pacotes ou alteração de vídeo; arquivo pode estar incompleto.",
                    );
                }
                if frame.has_new_parameters() {
                    match stream.streams()[index].parameters() {
                        Some(retina::codec::ParametersRef::Video(p)) if p.extra_data() == extra => {
                        }
                        _ => {
                            return Err(
                                "Parâmetros de vídeo mudaram; arquivo pode estar incompleto.",
                            )
                        }
                    }
                }
                if !started && !frame.is_random_access_point() {
                    continue;
                }
                started = true;
                let pts = frame.timestamp().elapsed_secs();
                let start = *origin.get_or_insert(pts);
                let ms = ((pts - start) * 1000.0).round() as i64;
                writer
                    .frame(ms, frame.data(), frame.is_random_access_point())
                    .map_err(|_| "Falha ao gravar vídeo; arquivo pode estar incompleto.")?;
            }
        }
    }
    writer
        .out
        .flush()
        .map_err(|_| "Falha ao finalizar arquivo; ele pode estar incompleto.")?;
    if writer.frames == 0 {
        return Err("Nenhum quadro exportado; confira o intervalo gravado.");
    }
    Ok("Exportação finalizada. Verifique o arquivo: a cobertura do intervalo não foi confirmada.")
}

async fn export_hik<W: Write>(
    mut session: Session<retina::client::Playing>,
    index: usize,
    key: &str,
    writer: &mut Writer<W>,
) -> Result<(), &'static str> {
    let mut depacketizer = hik::H265Rtp::new(key);
    let mut frame = Vec::new();
    let mut frame_pts: Option<f64> = None;
    let mut keyframe = false;
    let mut started = false;
    let mut origin = None;
    let mut errors = 0u32;
    let mut last = Instant::now();
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let next = tokio::time::timeout_at(deadline, session.next())
            .await
            .map_err(|_| {
                "Exportação interrompida por tempo esgotado; arquivo pode estar incompleto."
            })?;
        let Some(item) = next else { break };
        let packet = match item {
            Ok(PacketItem::Rtp(p)) => p,
            Ok(_) => continue,
            Err(e)
                if e.to_string()
                    .starts_with("Out-of-order packet or large loss")
                    && errors < 128
                    && last.elapsed() < Duration::from_secs(45) =>
            {
                errors += 1;
                depacketizer.discard_fragment();
                frame.clear();
                frame_pts = None;
                keyframe = false;
                continue;
            }
            Err(_) => return Err("Erro no fluxo; arquivo pode estar incompleto."),
        };
        if packet.stream_id() != index {
            continue;
        }
        deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        errors = 0;
        last = Instant::now();
        if packet.loss() > 0 {
            return Err("Pacotes perdidos; arquivo pode estar incompleto.");
        }
        let pts = packet.timestamp().elapsed_secs();
        if frame_pts.is_some_and(|p| p != pts) && !frame.is_empty() {
            return Err("Quadro incompleto; arquivo pode estar incompleto.");
        }
        frame_pts = Some(pts);
        for nal in depacketizer.push(
            packet.raw(),
            packet.payload(),
            packet.sequence_number(),
            packet.loss(),
        )? {
            let kind = (nal[0] >> 1) & 63;
            if (16..=21).contains(&kind) {
                keyframe = true;
            }
            append_nal(&mut frame, &nal)?;
        }
        if packet.mark() && !frame.is_empty() {
            if keyframe {
                started = true;
            }
            if started {
                let start = *origin.get_or_insert(pts);
                let ms = ((pts - start) * 1000.0).round() as i64;
                writer
                    .frame(ms, &frame, keyframe)
                    .map_err(|_| "Falha ao gravar vídeo; arquivo pode estar incompleto.")?;
            }
            frame.clear();
            frame_pts = None;
            keyframe = false;
        } else if packet.mark() {
            frame_pts = None;
            keyframe = false;
        }
    }
    if !frame.is_empty() {
        return Err("Fluxo terminou com quadro incompleto; arquivo pode estar incompleto.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_streaming_matroska_with_frame() {
        let mut writer =
            Writer::new(Vec::new(), "V_MPEGH/ISO/HEVC", &[1, 2, 3], 1920, 1080).unwrap();
        writer.frame(0, &[0, 0, 0, 2, 0x26, 1], true).unwrap();
        writer.frame(40, &[0, 0, 0, 2, 0x02, 1], false).unwrap();
        assert_eq!(writer.frames, 2);
        assert!(writer.out.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
        assert!(writer.out.windows(4).any(|b| b == [0x1f, 0x43, 0xb6, 0x75]));
    }
    #[test]
    fn frames_are_bounded_and_length_prefixed() {
        let mut frame = Vec::new();
        append_nal(&mut frame, &[1, 2, 3]).unwrap();
        assert_eq!(frame, &[0, 0, 0, 3, 1, 2, 3]);
        assert!(append_nal(&mut frame, &[]).is_err());
    }
}
