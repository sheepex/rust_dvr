use futures::StreamExt;
use retina::client::{
    Credentials, Demuxed, InitialSequenceNumberPolicy, PlayOptions, Playing, Session,
    SessionOptions, SetupOptions,
};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::{future::Future, pin::Pin};

#[path = "hik.rs"]
mod hik;
#[path = "playback_buffer.rs"]
mod playback_buffer;
pub use playback_buffer::PlaybackBuffer;

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub recording_elapsed: Option<f64>,
}

pub enum Event {
    Frame(Frame),
    // The diagnostic binary sends status through the same channel.
    #[allow(dead_code)]
    Status(&'static str),
}

fn convert(frame: &rust_h264::decoder::Frame) -> Option<Frame> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0 || h == 0 || w.checked_mul(h)? > 8_294_400 {
        return None;
    }
    let cw = frame.chroma_width() as usize;
    let ch = frame.chroma_height() as usize;
    if cw == 0 || ch == 0 {
        return None;
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    let shift = frame.bit_depth.saturating_sub(8);
    for y in 0..h {
        for x in 0..w {
            let ci = (y * ch / h) * cw + x * cw / w;
            let l = (frame.sample(&frame.y, y * w + x) >> shift) as f32;
            let u = (frame.sample(&frame.u, ci) >> shift) as f32 - 128.0;
            let v = (frame.sample(&frame.v, ci) >> shift) as f32 - 128.0;
            let l = (l - 16.0) * 1.164;
            rgb.push((l + 1.596 * v).clamp(0.0, 255.0) as u8);
            rgb.push((l - 0.392 * u - 0.813 * v).clamp(0.0, 255.0) as u8);
            rgb.push((l + 2.017 * u).clamp(0.0, 255.0) as u8);
        }
    }
    Some(Frame {
        width: w,
        height: h,
        rgb,
        recording_elapsed: None,
    })
}

fn convert_h265(frame: &rust_h265::decoder::Frame) -> Option<Frame> {
    convert_h265_planes(
        frame.width as usize,
        frame.height as usize,
        frame.bit_depth,
        frame.chroma_format_idc,
        [&frame.y, &frame.u, &frame.v],
    )
}

fn convert_h265_planes(
    w: usize,
    h: usize,
    bit_depth: u8,
    chroma_format: u32,
    planes: [&rust_h265::PixelData; 3],
) -> Option<Frame> {
    use rust_h265::PixelData;
    let [y_plane, u_plane, v_plane] = planes;
    if w == 0 || h == 0 || w.checked_mul(h)? > 8_294_400 || !(8..=12).contains(&bit_depth) {
        return None;
    }
    let (cw, ch) = match chroma_format {
        1 => (w.div_ceil(2), h.div_ceil(2)),
        3 => (w, h),
        _ => return None,
    };
    if y_plane.len() < w.checked_mul(h)?
        || u_plane.len() < cw.checked_mul(ch)?
        || v_plane.len() < cw.checked_mul(ch)?
    {
        return None;
    }
    let sample = |plane: &PixelData, index: usize| -> f32 {
        let value = match plane {
            PixelData::U8(data) => u16::from(data[index]),
            PixelData::U16(data) => data[index],
        };
        (value >> (bit_depth - 8)) as f32
    };
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let ci = (y * ch / h) * cw + x * cw / w;
            let l = (sample(y_plane, y * w + x) - 16.0) * 1.164;
            let u = sample(u_plane, ci) - 128.0;
            let v = sample(v_plane, ci) - 128.0;
            rgb.push((l + 1.596 * v).clamp(0.0, 255.0) as u8);
            rgb.push((l - 0.392 * u - 0.813 * v).clamp(0.0, 255.0) as u8);
            rgb.push((l + 2.017 * u).clamp(0.0, 255.0) as u8);
        }
    }
    Some(Frame {
        width: w,
        height: h,
        rgb,
        recording_elapsed: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_h265::PixelData;

    #[tokio::test]
    async fn compressed_reserve_builds_without_waiting_for_the_decoder() {
        use std::cell::RefCell;

        let timing = PlaybackTiming {
            rate: 1.0,
            origin: 0.0,
            prefill_seconds: 10.0,
            buffer: Default::default(),
        };
        let (source, input) = futures::channel::mpsc::unbounded();
        let (resume, paused) = tokio::sync::oneshot::channel();
        let decoded = RefCell::new(Vec::new());
        let observed = &decoded;
        let buffer = &timing.buffer;
        let pipeline =
            with_playback_reserve(Box::pin(input), Some(&timing), |mut input| async move {
                let first = input.next().await.unwrap()?;
                observed.borrow_mut().push(first.data);
                buffer
                    .push(Frame {
                        width: 1,
                        height: 1,
                        rgb: vec![0; 3],
                        recording_elapsed: Some(0.0),
                    })
                    .await;
                paused.await.unwrap();
                while let Some(unit) = input.next().await {
                    observed.borrow_mut().push(unit?.data);
                }
                Err(STREAM_ENDED)
            });
        tokio::pin!(pipeline);
        let send = |time| {
            source
                .unbounded_send(Ok(Encoded {
                    data: time,
                    bytes: 1024,
                    elapsed: Some(time),
                }))
                .unwrap()
        };
        for time in [0.0, 4.0, 9.0] {
            send(time);
        }
        // The first poll may visit the decoder before forwarding the source data.
        assert!(futures::poll!(&mut pipeline).is_pending());
        assert!(futures::poll!(&mut pipeline).is_pending());
        assert!(!timing.buffer.ready());
        assert_eq!(*decoded.borrow(), [0.0]);

        send(10.0);
        assert!(futures::poll!(&mut pipeline).is_pending());
        assert!(timing.buffer.ready());
        assert_eq!(*decoded.borrow(), [0.0]);

        // While the network sends nothing, the reserve can feed the decoder in order.
        resume.send(()).unwrap();
        assert!(futures::poll!(&mut pipeline).is_pending());
        assert_eq!(*decoded.borrow(), [0.0, 4.0, 9.0, 10.0]);
        source.unbounded_send(Err("Falha no vídeo.")).unwrap();
        assert_eq!(pipeline.await, Err("Falha no vídeo."));
    }

    #[test]
    fn absolute_rtsp_range_anchors_playback_to_the_reported_time() {
        let requested = "2024-03-01T00:00:00".parse().unwrap();
        assert_eq!(
            range_offset("clock=20240301T000010.5Z-", requested),
            Some(10.5)
        );
        assert_eq!(
            range_offset("clock=20240229T235958Z-20240301T003000Z", requested),
            Some(-2.0)
        );
        assert_eq!(range_offset("npt=0.000-", requested), None);
        assert_eq!(
            range_offset("clock=20240301T000010garbageZ-", requested),
            None
        );
    }

    #[test]
    fn converts_h265_420_planes_at_eight_and_ten_bits() {
        let y8 = PixelData::U8(vec![16, 235, 16, 235]);
        let u8 = PixelData::U8(vec![128]);
        let v8 = PixelData::U8(vec![128]);
        let y10 = PixelData::U16(vec![64, 940, 64, 940]);
        let u10 = PixelData::U16(vec![512]);
        let v10 = PixelData::U16(vec![512]);
        let image8 = convert_h265_planes(2, 2, 8, 1, [&y8, &u8, &v8]).unwrap();
        let image10 = convert_h265_planes(2, 2, 10, 1, [&y10, &u10, &v10]).unwrap();
        assert_eq!(image8.rgb, image10.rgb);
        assert_eq!(&image8.rgb[..3], &[0, 0, 0]);
        assert!(image8.rgb[3] >= 254);
    }

    #[test]
    fn rejects_unsupported_chroma_and_short_planes() {
        let plane = PixelData::U8(vec![128]);
        assert!(convert_h265_planes(2, 2, 8, 2, [&plane, &plane, &plane]).is_none());
        assert!(convert_h265_planes(2, 2, 8, 1, [&plane, &plane, &plane]).is_none());
    }

    #[test]
    fn reports_invalid_h265_parameters_without_exposing_stream_details() {
        let error = "sps_max_sub_layers_minus1 must be in [0, 6]\nprivate stream data";
        assert_eq!(
            receive_error("h265", error),
            "Parâmetros H.265 inválidos. O fluxo pode estar criptografado ou ser incompatível."
        );
        assert_eq!(
            receive_error("h264", error),
            "Erro ao receber o vídeo RTSP (código R0)."
        );
        assert_eq!(
            receive_error("h265", "outro erro"),
            "Erro ao receber o vídeo RTSP (código R0)."
        );
    }

    #[test]
    fn reports_rtsp_status_without_exposing_details() {
        assert!(describe_error(Some(401)).contains("Autenticação"));
        assert!(describe_error(Some(403)).contains("permissão"));
        assert!(describe_error(Some(404)).contains("não encontrado"));
        assert!(describe_error(None).contains("Falha RTSP"));
    }

    #[test]
    fn retries_connection_failures_but_not_rejected_credentials_or_unsupported_video() {
        assert!(retryable("Tempo esgotado sem receber vídeo."));
        assert!(retryable("A transmissão foi encerrada."));
        assert!(retryable(receive_error(
            "h264",
            "Error reading from RTSP peer: reset"
        )));
        for code in [401, 403, 404] {
            assert!(!retryable(request_error(
                Some(code),
                "Falha ao preparar o vídeo RTSP."
            )));
        }
        assert!(!retryable("Codec não suportado: use H.264 ou H.265."));
    }

    #[test]
    fn classifies_receive_errors_without_exposing_private_context() {
        let secret = "\n\nconn: private address\npkt: private data";
        for (detail, expected) in [
            ("wrong ssrc after 2 RTP pkts", "identificador RTP"),
            ("Out-of-order packet or large loss; expecting seq=Some(Seq { init: PlayResponseHeader, next: 123 })", "R2-início"),
            ("Out-of-order packet or large loss; expecting seq=Some(Seq { init: RtpPacket, next: 123 })", "R2-contínuo"),
            ("Out-of-order packet or large loss; expecting ssrc=Some(Ssrc { init: PlayResponseHeader, ssrc: 0x1 }) seq=Some(Seq { init: RtpPacket, next: 123 })", "R2-contínuo"),
            ("Out-of-order packet or large loss", "R2"),
            ("Timestamp jumped 100", "horário RTP"),
            (
                "Error reading from RTSP peer: connection reset",
                "conexão RTSP",
            ),
            ("RTSP framing error: invalid data", "mensagem RTSP"),
            ("corrupt RTP header", "cabeçalho RTP"),
            ("something unexpected", "código R0"),
        ] {
            let result = receive_error("h265", &format!("{detail}{secret}"));
            assert!(result.contains(expected), "{result}");
            assert!(!result.contains("private"));
        }
    }

    #[test]
    fn identifies_only_hikvision_recorded_tracks_for_initial_sequence_workaround() {
        assert!(is_hik_playback("/Streaming/tracks/101"));
        assert!(!is_hik_playback("/Streaming/Channels/101"));
        assert!(!is_hik_playback("/cam/playback"));
    }
}

fn receive_error(codec: &str, detail: &str) -> &'static str {
    if codec == "h265" && detail.contains("sps_max_sub_layers_minus1 must be in [0, 6]") {
        "Parâmetros H.265 inválidos. O fluxo pode estar criptografado ou ser incompatível."
    } else if detail.starts_with("wrong ssrc ") {
        #[cfg(test)]
        if std::env::var_os("ONVIF_TEST_RTP_DIAGNOSTIC").is_some() {
            eprintln!(
                "Diagnóstico RTP: {}.",
                detail
                    .split("; expecting")
                    .next()
                    .unwrap_or("identificador inconsistente")
            );
        }
        "Erro de identificador RTP (R1) durante a reprodução ou transmissão."
    } else if detail.starts_with("Out-of-order packet or large loss") {
        if detail.contains("seq=Some(Seq { init: PlayResponseHeader") {
            "Erro de sequência RTP (R2-início): sequência inicial incompatível."
        } else if detail.contains("seq=Some(Seq { init: RtpPacket") {
            "Erro de sequência RTP (R2-contínuo): descontinuidade após o início."
        } else {
            "Erro de sequência RTP (R2): pacotes fora de ordem ou perda elevada."
        }
    } else if detail.starts_with("Timestamp jumped ") {
        "Erro de horário RTP (R3): salto no tempo dos pacotes."
    } else if detail.starts_with("Error reading from RTSP peer:") {
        "Erro de leitura da conexão RTSP (R4): a conexão foi interrompida."
    } else if detail.starts_with("RTSP framing error:") {
        "Erro de mensagem RTSP (R5): resposta inesperada do equipamento."
    } else if detail.starts_with("corrupt RTP header") {
        "Erro de cabeçalho RTP (R6): pacote inválido recebido."
    } else {
        "Erro ao receber o vídeo RTSP (código R0)."
    }
}

fn is_hik_playback(path: &str) -> bool {
    path.to_ascii_lowercase().starts_with("/streaming/tracks/")
}

fn describe_error(status: Option<u16>) -> &'static str {
    match status {
        Some(401) => "Autenticação RTSP recusada; confira usuário e senha.",
        Some(403) => "Acesso RTSP negado; confira a permissão do usuário.",
        Some(404) => "Canal ou gravação não encontrado; confira canal e intervalo.",
        _ => "Falha RTSP: confira endereço, porta, canal, usuário e senha.",
    }
}

fn request_error(status: Option<u16>, fallback: &'static str) -> &'static str {
    match status {
        Some(401 | 403 | 404) => describe_error(status),
        _ => fallback,
    }
}

pub fn retryable(error: &str) -> bool {
    error.starts_with("Tempo esgotado")
        || error.starts_with("Falha RTSP:")
        || error == "Falha ao preparar o vídeo RTSP."
        || error == "O DVR recusou iniciar a transmissão."
        || error == "A transmissão foi encerrada."
        || error.starts_with("Erro de ")
        || error == "Vídeo recebido, mas não foi possível decodificar quadros."
        || error
            == "Não foi possível decodificar o fluxo Hikvision; confira o código de verificação."
}

fn h265_decode_nal(
    decoder: &mut rust_h265::Decoder,
    nal: &rust_h265::NalUnit<'_>,
) -> Result<(), &'static str> {
    match decoder.decode_nal(nal) {
        Ok(_) => Ok(()),
        Err(rust_h265::DecodeError::Unsupported(_)) => {
            Err("Perfil ou recurso H.265 não suportado pelo decodificador.")
        }
        Err(rust_h265::DecodeError::ResourceLimit(_)) => {
            Err("O vídeo H.265 excede os limites do decodificador.")
        }
        // Corrupt or incomplete packets may recover at the next keyframe.
        Err(_) => Ok(()),
    }
}

async fn send_frame(
    frames: &SyncSender<Event>,
    ctx: &Option<eframe::egui::Context>,
    mut image: Frame,
    timestamp: retina::Timestamp,
    playback: Option<&PlaybackTiming>,
) -> bool {
    if let Some(playback) = playback {
        // RTP uses the playback clock; Scale relates it to the recording position.
        image.recording_elapsed = Some(playback.origin + timestamp.elapsed_secs() * playback.rate);
        playback.buffer.push(image).await;
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
        return true;
    }
    match frames.try_send(Event::Frame(image)) {
        Ok(()) => {
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
            true
        }
        Err(std::sync::mpsc::TrySendError::Full(_)) => true,
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
    }
}

#[derive(Clone)]
pub struct PlaybackOptions {
    pub rate: f64,
    pub sessions: Arc<retina::client::SessionGroup>,
    pub requested_utc: Option<jiff::civil::DateTime>,
    pub buffer: Arc<PlaybackBuffer>,
    pub prefill_seconds: f64,
    pub onvif_range: Option<String>,
}

struct PlaybackTiming {
    rate: f64,
    origin: f64,
    buffer: Arc<PlaybackBuffer>,
    prefill_seconds: f64,
}

const ENCODED_BYTES: usize = 32 * 1024 * 1024;
const ENCODED_ITEMS: usize = 65_536;
pub const STREAM_ENDED: &str = "A transmissão foi encerrada.";

struct Encoded<T> {
    data: T,
    bytes: usize,
    elapsed: Option<f64>,
}

type Input<'a, T> = Pin<Box<dyn futures::Stream<Item = Result<Encoded<T>, &'static str>> + 'a>>;

/// Run the network reader and decoder concurrently only for recorded video.
/// The reader can build a compressed reserve while the RGB queue paces the decoder.
async fn with_playback_reserve<'a, T: 'a, F>(
    mut input: Input<'a, T>,
    playback: Option<&PlaybackTiming>,
    decode: impl FnOnce(Input<'a, T>) -> F,
) -> Result<(), &'static str>
where
    F: Future<Output = Result<(), &'static str>>,
{
    let Some(playback) = playback else {
        return decode(input).await;
    };
    let byte_budget = Arc::new(tokio::sync::Semaphore::new(ENCODED_BYTES));
    let (sender, receiver) = tokio::sync::mpsc::channel(ENCODED_ITEMS);
    let receiver = futures::stream::unfold(receiver, |mut receiver| async {
        let (unit, _budget) = receiver.recv().await?;
        Some((Ok(unit), receiver))
    });
    let reader = async move {
        let mut first = None;
        while let Some(item) = input.next().await {
            let unit = item?;
            if unit.bytes > ENCODED_BYTES {
                return Err("O quadro comprimido excede o limite do buffer.");
            }
            let bytes = unit.bytes as u32;
            let permit = match byte_budget.clone().try_acquire_many_owned(bytes) {
                Ok(permit) => permit,
                Err(_) => {
                    playback.buffer.mark_source_ready();
                    let Ok(permit) = byte_budget.clone().acquire_many_owned(bytes).await else {
                        break;
                    };
                    permit
                }
            };
            let elapsed = unit.elapsed.filter(|time| time.is_finite());
            if sender.send((unit, permit)).await.is_err() {
                break;
            }
            if let Some(elapsed) = elapsed {
                let first = *first.get_or_insert(elapsed);
                if elapsed - first >= playback.prefill_seconds {
                    playback.buffer.mark_source_ready();
                }
            }
            if sender.capacity() == 0 {
                playback.buffer.mark_source_ready();
            }
        }
        Ok(())
    };
    let decoder = decode(Box::pin(receiver));
    tokio::pin!(decoder);
    tokio::select! {
        result = &mut decoder => result,
        result = reader => {
            // Let queued frames play even when the source reaches EOF or fails.
            playback.buffer.mark_source_ready();
            let decoded = decoder.await;
            if decoded == Err(STREAM_ENDED) {
                result.and(decoded)
            } else {
                decoded.and(result)
            }
        }
    }
}

async fn next_encoded<T>(
    input: &mut Input<'_, T>,
    last_decoded: Instant,
    decode_timeout: u64,
) -> Result<T, &'static str> {
    let deadline =
        tokio::time::Instant::from_std(last_decoded + Duration::from_secs(decode_timeout))
            .min(tokio::time::Instant::now() + Duration::from_secs(20));
    tokio::time::timeout_at(deadline, input.next())
        .await
        .map_err(|_| "Tempo esgotado sem receber vídeo.")?
        .ok_or(STREAM_ENDED)?
        .map(|unit| unit.data)
}

fn range_offset(range: &str, requested: jiff::civil::DateTime) -> Option<f64> {
    let (start, _) = range.trim().strip_prefix("clock=")?.split_once("Z-")?;
    if !start.is_ascii() || start.len() < 15 || &start[8..9] != "T" {
        return None;
    }
    let actual: jiff::civil::DateTime = format!(
        "{}-{}-{}T{}:{}:{}{}",
        &start[..4],
        &start[4..6],
        &start[6..8],
        &start[9..11],
        &start[11..13],
        &start[13..15],
        &start[15..],
    )
    .parse()
    .ok()?;
    Some(requested.duration_until(actual).as_secs_f64())
}

pub async fn run(
    url: url::Url,
    credentials: Credentials,
    frames: SyncSender<Event>,
    ctx: Option<eframe::egui::Context>,
    playback: Option<PlaybackOptions>,
    onvif: bool,
) -> Result<(), &'static str> {
    let hik_key = credentials.password.clone();
    let path = url.path().to_ascii_lowercase();
    let hik_stream =
        path.starts_with("/streaming/channels/") || path.starts_with("/streaming/tracks/");
    let options = SessionOptions::default().creds(Some(credentials));
    let options = if let Some(playback) = &playback {
        options.session_group(playback.sessions.clone())
    } else {
        options
    };
    let playback_scale = playback.as_ref().map(|playback| playback.rate);
    let mut session =
        tokio::time::timeout(Duration::from_secs(12), Session::describe(url, options))
            .await
            .map_err(|_| "Tempo esgotado ao conectar ao DVR.")?
            .map_err(|error| describe_error(error.status_code()))?;
    let index = session
        .streams()
        .iter()
        .position(|s| s.media() == "video")
        .ok_or("O equipamento não anunciou vídeo nesse canal.")?;
    let codec = session.streams()[index]
        .encoding_name()
        .to_ascii_lowercase();
    #[cfg(test)]
    if onvif && playback.is_some() {
        eprintln!("Diagnóstico de reprodução: codec {codec}.");
    }
    if codec != "h264" && codec != "h265" {
        return Err("Codec não suportado: use H.264 ou H.265.");
    }
    tokio::time::timeout(
        Duration::from_secs(12),
        session.setup(
            index,
            SetupOptions::default().onvif_replay(
                playback
                    .as_ref()
                    .is_some_and(|options| options.onvif_range.is_some()),
            ),
        ),
    )
    .await
    .map_err(|_| "Tempo esgotado ao preparar o vídeo.")?
    .map_err(|error| request_error(error.status_code(), "Falha ao preparar o vídeo RTSP."))?;
    let play_options = if onvif || is_hik_playback(&path) {
        // Some recorders report an initial sequence unrelated to the first RTP packet.
        // Keep Retina's normal validation once the stream has started.
        PlayOptions::default().initial_seq(InitialSequenceNumberPolicy::Ignore)
    } else {
        PlayOptions::default()
    };
    let (session, playback_timing) = tokio::time::timeout(Duration::from_secs(12), async {
        if let Some(playback) = playback {
            let scale = playback.rate;
            let scale_request = (scale != 1.0).then_some(scale);
            let (session, reported_scale, range) = if let Some(range) = &playback.onvif_range {
                session
                    .play_with_timing_range(play_options, scale_request, range)
                    .await?
            } else {
                session
                    .play_with_timing(play_options, scale_request)
                    .await?
            };
            #[cfg(test)]
            if playback.onvif_range.is_some() {
                eprintln!(
                    "Diagnóstico: servidor respondeu com Range: {}.",
                    range.is_some()
                );
            }
            if playback.onvif_range.is_some() && range.is_none() {
                if frames
                    .try_send(Event::Status(
                        "A câmera não confirmou o horário da gravação; confira as imagens.",
                    ))
                    .is_ok()
                {
                    if let Some(ctx) = &ctx {
                        ctx.request_repaint();
                    }
                }
            }
            let notice = match (scale != 1.0, reported_scale) {
                (true, None) => Some("O DVR não confirmou a velocidade solicitada."),
                (true, Some(actual)) if (actual - scale).abs() > 0.001 => {
                    Some("O DVR informou uma velocidade diferente da solicitada.")
                }
                _ => None,
            };
            if let Some(notice) = notice {
                if frames.try_send(Event::Status(notice)).is_ok() {
                    if let Some(ctx) = &ctx {
                        ctx.request_repaint();
                    }
                }
            }
            Ok((
                session,
                Some(PlaybackTiming {
                    rate: reported_scale.filter(|value| *value > 0.0).unwrap_or(1.0),
                    origin: playback
                        .requested_utc
                        .zip(range)
                        .and_then(|(requested, range)| range_offset(&range, requested))
                        .unwrap_or(0.0),
                    buffer: playback.buffer,
                    prefill_seconds: playback.prefill_seconds,
                }),
            ))
        } else {
            session
                .play(play_options)
                .await
                .map(|session| (session, None))
        }
    })
    .await
    .map_err(|_| "Tempo esgotado ao iniciar o vídeo.")?
    .map_err(|error| {
        request_error(
            error.status_code(),
            match (
                playback_scale.filter(|scale| *scale != 1.0),
                error.status_code(),
            ) {
                (Some(_), Some(code)) if (400..500).contains(&code) || code == 501 => {
                    "O DVR recusou a reprodução nessa velocidade. Tente 1× ou outra velocidade."
                }
                _ => "O DVR recusou iniciar a transmissão.",
            },
        )
    })?;
    if hik_stream && codec == "h265" {
        return run_hik_h265(
            session,
            index,
            &hik_key,
            frames,
            ctx,
            is_hik_playback(&path),
            playback_timing,
        )
        .await;
    }
    let stream = session
        .demuxed()
        .map_err(|_| "Formato de vídeo RTSP incompatível.")?;
    let read_codec = codec.as_str();
    let input = futures::stream::unfold(
        (stream, Vec::new()),
        move |(mut stream, mut extra)| async move {
            let unit = read_demuxed(&mut stream, index, read_codec, &mut extra).await;
            Some((unit, (stream, extra)))
        },
    );
    with_playback_reserve(Box::pin(input), playback_timing.as_ref(), |input| {
        decode_demuxed(
            input,
            codec == "h264",
            &frames,
            &ctx,
            playback_timing.as_ref(),
        )
    })
    .await
}

enum DecoderConfig {
    H264(Vec<u8>),
    H265([Vec<u8>; 3]),
}

struct DemuxedFrame {
    timestamp: retina::Timestamp,
    frame: Vec<u8>,
    config: Option<DecoderConfig>,
}

async fn read_demuxed(
    stream: &mut Demuxed,
    index: usize,
    codec: &str,
    extra: &mut Vec<u8>,
) -> Result<Encoded<DemuxedFrame>, &'static str> {
    loop {
        let item = stream
            .next()
            .await
            .ok_or(STREAM_ENDED)?
            .map_err(|error| receive_error(codec, &error.to_string()))?;
        let retina::codec::CodecItem::VideoFrame(frame) = item else {
            continue;
        };
        if frame.stream_id() != index {
            continue;
        }
        // Snapshot configuration changes in stream order, before reading another frame.
        let config = match stream.streams()[index].parameters() {
            Some(retina::codec::ParametersRef::Video(params))
                if params.extra_data() != extra.as_slice() =>
            {
                *extra = params.extra_data().to_vec();
                if codec == "h264" {
                    Some(DecoderConfig::H264(extra.clone()))
                } else if let retina::codec::VideoParametersCodec::H265 { vps, sps, pps } =
                    params.codec_params()
                {
                    Some(DecoderConfig::H265(
                        [vps, sps, pps].map(|bytes| bytes.to_vec()),
                    ))
                } else {
                    None
                }
            }
            _ => None,
        };
        let timestamp = frame.timestamp();
        let frame = frame.into_data();
        let config_bytes = match &config {
            Some(DecoderConfig::H264(extra)) => extra.capacity(),
            Some(DecoderConfig::H265(params)) => params.iter().map(Vec::capacity).sum(),
            None => 0,
        };
        return Ok(Encoded {
            bytes: frame.capacity().saturating_add(config_bytes),
            elapsed: Some(timestamp.elapsed_secs()),
            data: DemuxedFrame {
                timestamp,
                frame,
                config,
            },
        });
    }
}

async fn decode_demuxed(
    mut input: Input<'_, DemuxedFrame>,
    h264: bool,
    frames: &SyncSender<Event>,
    ctx: &Option<eframe::egui::Context>,
    playback_timing: Option<&PlaybackTiming>,
) -> Result<(), &'static str> {
    let mut decoder = rust_h264::decoder::Decoder::new();
    let mut decoder_h265 = rust_h265::Decoder::new().with_display_order();
    let mut last_decoded = Instant::now();
    loop {
        let DemuxedFrame {
            timestamp,
            frame,
            config,
        } = next_encoded(&mut input, last_decoded, 30).await?;
        match config {
            Some(DecoderConfig::H264(extra)) => {
                if let Ok(config) = rust_h264::nal::parse_avcc_config(&extra) {
                    for nal in config.sps_nals.iter().chain(config.pps_nals.iter()) {
                        let _ = decoder.decode_nal(nal);
                    }
                }
            }
            Some(DecoderConfig::H265(params)) => {
                decoder_h265 = rust_h265::Decoder::new().with_display_order();
                for data in &params {
                    let nal = rust_h265::parse_nal(data).ok_or("Parâmetros H.265 inválidos.")?;
                    h265_decode_nal(&mut decoder_h265, &nal)?;
                }
            }
            None => {}
        }
        if h264 {
            for nal in rust_h264::nal::parse_avcc(&frame, 4) {
                if let Ok(Some(decoded)) = decoder.decode_nal(&nal) {
                    if let Some(image) = convert(&decoded) {
                        if !send_frame(frames, ctx, image, timestamp, playback_timing).await {
                            return Ok(());
                        }
                        last_decoded = Instant::now();
                    }
                }
            }
        } else {
            for nal in rust_h265::parse_hvcc(&frame, 4) {
                h265_decode_nal(&mut decoder_h265, &nal)?;
                while let Some(decoded) = decoder_h265.next_output() {
                    let image =
                        convert_h265(&decoded).ok_or("Formato de imagem H.265 não suportado.")?;
                    if !send_frame(frames, ctx, image, timestamp, playback_timing).await {
                        return Ok(());
                    }
                    last_decoded = Instant::now();
                }
            }
        }
        if last_decoded.elapsed() > Duration::from_secs(30) {
            return Err("Vídeo recebido, mas não foi possível decodificar quadros.");
        }
    }
}

async fn run_hik_h265(
    session: Session<Playing>,
    index: usize,
    key: &str,
    frames: SyncSender<Event>,
    ctx: Option<eframe::egui::Context>,
    playback: bool,
    playback_timing: Option<PlaybackTiming>,
) -> Result<(), &'static str> {
    let mut decoder = rust_h265::Decoder::new().with_display_order();
    if let Some(retina::codec::ParametersRef::Video(params)) = session.streams()[index].parameters()
    {
        if let retina::codec::VideoParametersCodec::H265 { vps, sps, pps } = params.codec_params() {
            for data in [vps, sps, pps] {
                if let Some(nal) = rust_h265::parse_nal(data) {
                    h265_decode_nal(&mut decoder, &nal)?;
                }
            }
        }
    }
    let input = futures::stream::unfold(session, move |mut session| async move {
        let unit = read_hik(&mut session, index, playback).await;
        Some((unit, session))
    });
    with_playback_reserve(Box::pin(input), playback_timing.as_ref(), |input| {
        decode_hik(input, key, decoder, &frames, &ctx, playback_timing.as_ref())
    })
    .await
}

enum HikInput {
    Packet(retina::rtp::ReceivedPacket),
    LostPacket,
}

async fn read_hik(
    session: &mut Session<Playing>,
    index: usize,
    playback: bool,
) -> Result<Encoded<HikInput>, &'static str> {
    loop {
        match session.next().await.ok_or(STREAM_ENDED)? {
            Ok(retina::client::PacketItem::Rtp(packet)) if packet.stream_id() == index => {
                return Ok(Encoded {
                    bytes: packet.raw().len(),
                    elapsed: Some(packet.timestamp().elapsed_secs()),
                    data: HikInput::Packet(packet),
                });
            }
            Ok(_) => {}
            Err(error) => {
                let detail = error.to_string();
                if playback && detail.starts_with("Out-of-order packet or large loss") {
                    return Ok(Encoded {
                        data: HikInput::LostPacket,
                        bytes: 0,
                        elapsed: None,
                    });
                }
                return Err(receive_error("h265", &detail));
            }
        }
    }
}

async fn decode_hik(
    mut input: Input<'_, HikInput>,
    key: &str,
    mut decoder: rust_h265::Decoder,
    frames: &SyncSender<Event>,
    ctx: &Option<eframe::egui::Context>,
    playback_timing: Option<&PlaybackTiming>,
) -> Result<(), &'static str> {
    let mut depacketizer = hik::H265Rtp::new(key);
    let mut last_decoded = Instant::now();
    let mut sequence_errors = 0u32;
    loop {
        let packet = match next_encoded(&mut input, last_decoded, 45).await? {
            HikInput::Packet(packet) => packet,
            HikInput::LostPacket => {
                depacketizer.discard_fragment();
                sequence_errors += 1;
                if sequence_errors <= 128 && last_decoded.elapsed() < Duration::from_secs(45) {
                    continue;
                }
                return Err(receive_error("h265", "Out-of-order packet or large loss"));
            }
        };
        sequence_errors = 0;
        for data in depacketizer.push(
            packet.raw(),
            packet.payload(),
            packet.sequence_number(),
            packet.loss(),
        )? {
            if let Some(nal) = rust_h265::parse_nal(&data) {
                h265_decode_nal(&mut decoder, &nal)?;
                while let Some(decoded) = decoder.next_output() {
                    let image =
                        convert_h265(&decoded).ok_or("Formato de imagem H.265 não suportado.")?;
                    if !send_frame(frames, ctx, image, packet.timestamp(), playback_timing).await {
                        return Ok(());
                    }
                    last_decoded = Instant::now();
                }
            }
        }
        if last_decoded.elapsed() > Duration::from_secs(45) {
            return Err(
                "Não foi possível decodificar o fluxo Hikvision; confira o código de verificação.",
            );
        }
    }
}
