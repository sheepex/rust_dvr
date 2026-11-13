use futures::StreamExt;
use retina::client::{Credentials, PlayOptions, Session, SessionOptions, SetupOptions};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

pub enum Event {
    Frame(Frame),
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
    })
}

pub async fn run(
    url: url::Url,
    credentials: Credentials,
    frames: SyncSender<Event>,
    ctx: Option<eframe::egui::Context>,
) -> Result<(), &'static str> {
    let options = SessionOptions::default().creds(Some(credentials));
    let mut session =
        tokio::time::timeout(Duration::from_secs(12), Session::describe(url, options))
            .await
            .map_err(|_| "Tempo esgotado ao conectar ao DVR.")?
            .map_err(|_| "Falha RTSP: confira endereço, porta, canal, usuário e senha.")?;
    let index = session
        .streams()
        .iter()
        .position(|s| s.media() == "video")
        .ok_or("O equipamento não anunciou vídeo nesse canal.")?;
    if session.streams()[index].encoding_name().to_lowercase() != "h264" {
        return Err("Codec não suportado nesta versão: somente H.264 integrado; H.265 ainda não implementado.");
    }
    tokio::time::timeout(
        Duration::from_secs(12),
        session.setup(index, SetupOptions::default()),
    )
    .await
    .map_err(|_| "Tempo esgotado ao preparar o vídeo.")?
    .map_err(|_| "Falha ao preparar o vídeo RTSP.")?;
    let session = tokio::time::timeout(
        Duration::from_secs(12),
        session.play(PlayOptions::default()),
    )
    .await
    .map_err(|_| "Tempo esgotado ao iniciar o vídeo.")?
    .map_err(|_| "O DVR recusou iniciar a transmissão.")?;
    let mut stream = session
        .demuxed()
        .map_err(|_| "Formato de vídeo RTSP incompatível.")?;
    let mut decoder = rust_h264::decoder::Decoder::new();
    let mut extra = Vec::new();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(20), stream.next())
            .await
            .map_err(|_| "Tempo esgotado sem receber vídeo.")?
            .ok_or("A transmissão foi encerrada.")?
            .map_err(|_| "Erro ao receber o vídeo RTSP.")?;
        if let retina::codec::CodecItem::VideoFrame(frame) = item {
            if let Some(retina::codec::ParametersRef::Video(params)) =
                stream.streams()[index].parameters()
            {
                if params.extra_data() != extra {
                    extra = params.extra_data().to_vec();
                    if let Ok(config) = rust_h264::nal::parse_avcc_config(&extra) {
                        for nal in config.sps_nals.iter().chain(config.pps_nals.iter()) {
                            let _ = decoder.decode_nal(nal);
                        }
                    }
                }
            }
            for nal in rust_h264::nal::parse_avcc(frame.data(), 4) {
                if let Ok(Some(decoded)) = decoder.decode_nal(&nal) {
                    if let Some(image) = convert(&decoded) {
                        match frames.try_send(Event::Frame(image)) {
                            Ok(()) => {
                                if let Some(ctx) = &ctx {
                                    ctx.request_repaint();
                                }
                            }
                            Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return Ok(()),
                        }
                    }
                }
            }
        }
    }
}
