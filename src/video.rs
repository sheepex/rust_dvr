use futures::StreamExt;
use retina::client::{Credentials, PlayOptions, Session, SessionOptions, SetupOptions};
use std::sync::mpsc::SyncSender;
use std::time::Duration;
use std::time::Instant;

#[path = "hik.rs"]
mod hik;

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_h265::PixelData;

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
            "Erro ao receber o vídeo RTSP."
        );
        assert_eq!(
            receive_error("h265", "outro erro"),
            "Erro ao receber o vídeo RTSP."
        );
    }
}

fn receive_error(codec: &str, detail: &str) -> &'static str {
    if codec == "h265" && detail.contains("sps_max_sub_layers_minus1 must be in [0, 6]") {
        "Parâmetros H.265 inválidos. O fluxo pode estar criptografado ou ser incompatível."
    } else {
        "Erro ao receber o vídeo RTSP."
    }
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

fn send_frame(
    frames: &SyncSender<Event>,
    ctx: &Option<eframe::egui::Context>,
    image: Frame,
) -> bool {
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

pub async fn run(
    url: url::Url,
    credentials: Credentials,
    frames: SyncSender<Event>,
    ctx: Option<eframe::egui::Context>,
) -> Result<(), &'static str> {
    let hik_key = credentials.password.clone();
    let hik_live = url
        .path()
        .to_ascii_lowercase()
        .starts_with("/streaming/channels/");
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
    let codec = session.streams()[index]
        .encoding_name()
        .to_ascii_lowercase();
    if codec != "h264" && codec != "h265" {
        return Err("Codec não suportado: use H.264 ou H.265.");
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
    if hik_live && codec == "h265" {
        return run_hik_h265(session, index, &hik_key, frames, ctx).await;
    }
    let mut stream = session
        .demuxed()
        .map_err(|_| "Formato de vídeo RTSP incompatível.")?;
    let mut decoder = rust_h264::decoder::Decoder::new();
    let mut decoder_h265 = rust_h265::Decoder::new().with_display_order();
    let mut extra = Vec::new();
    let mut last_decoded = Instant::now();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(20), stream.next())
            .await
            .map_err(|_| "Tempo esgotado sem receber vídeo.")?
            .ok_or("A transmissão foi encerrada.")?
            .map_err(|error| receive_error(&codec, &error.to_string()))?;
        if let retina::codec::CodecItem::VideoFrame(frame) = item {
            if frame.stream_id() != index {
                continue;
            }
            if let Some(retina::codec::ParametersRef::Video(params)) =
                stream.streams()[index].parameters()
            {
                if params.extra_data() != extra {
                    extra = params.extra_data().to_vec();
                    if codec == "h264" {
                        if let Ok(config) = rust_h264::nal::parse_avcc_config(&extra) {
                            for nal in config.sps_nals.iter().chain(config.pps_nals.iter()) {
                                let _ = decoder.decode_nal(nal);
                            }
                        }
                    } else if let retina::codec::VideoParametersCodec::H265 { vps, sps, pps } =
                        params.codec_params()
                    {
                        decoder_h265 = rust_h265::Decoder::new().with_display_order();
                        for data in [vps, sps, pps] {
                            let nal =
                                rust_h265::parse_nal(data).ok_or("Parâmetros H.265 inválidos.")?;
                            h265_decode_nal(&mut decoder_h265, &nal)?;
                        }
                    }
                }
            }
            if codec == "h264" {
                for nal in rust_h264::nal::parse_avcc(frame.data(), 4) {
                    if let Ok(Some(decoded)) = decoder.decode_nal(&nal) {
                        if let Some(image) = convert(&decoded) {
                            last_decoded = Instant::now();
                            if !send_frame(&frames, &ctx, image) {
                                return Ok(());
                            }
                        }
                    }
                }
            } else {
                for nal in rust_h265::parse_hvcc(frame.data(), 4) {
                    h265_decode_nal(&mut decoder_h265, &nal)?;
                    while let Some(decoded) = decoder_h265.next_output() {
                        let image = convert_h265(&decoded)
                            .ok_or("Formato de imagem H.265 não suportado.")?;
                        last_decoded = Instant::now();
                        if !send_frame(&frames, &ctx, image) {
                            return Ok(());
                        }
                    }
                }
            }
            if last_decoded.elapsed() > Duration::from_secs(30) {
                return Err("Vídeo recebido, mas não foi possível decodificar quadros.");
            }
        }
    }
}

async fn run_hik_h265(
    mut session: retina::client::Session<retina::client::Playing>,
    index: usize,
    key: &str,
    frames: SyncSender<Event>,
    ctx: Option<eframe::egui::Context>,
) -> Result<(), &'static str> {
    let mut depacketizer = hik::H265Rtp::new(key);
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
    let mut last_decoded = Instant::now();
    loop {
        let packet = tokio::time::timeout(Duration::from_secs(20), session.next())
            .await
            .map_err(|_| "Tempo esgotado sem receber vídeo.")?
            .ok_or("A transmissão foi encerrada.")?
            .map_err(|_| "Erro ao receber o vídeo RTSP.")?;
        if let retina::client::PacketItem::Rtp(packet) = packet {
            if packet.stream_id() != index {
                continue;
            }
            for data in depacketizer.push(
                packet.raw(),
                packet.payload(),
                packet.sequence_number(),
                packet.loss(),
            )? {
                if let Some(nal) = rust_h265::parse_nal(&data) {
                    h265_decode_nal(&mut decoder, &nal)?;
                    while let Some(decoded) = decoder.next_output() {
                        let image = convert_h265(&decoded)
                            .ok_or("Formato de imagem H.265 não suportado.")?;
                        last_decoded = Instant::now();
                        if !send_frame(&frames, &ctx, image) {
                            return Ok(());
                        }
                    }
                }
            }
            if last_decoded.elapsed() > Duration::from_secs(45) {
                return Err("Não foi possível decodificar o fluxo Hikvision; confira o código de verificação.");
            }
        }
    }
}
