use eframe::egui;
mod config;
mod discovery;
mod export;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod maintenance;
mod onvif;
mod playback;
mod video;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::{
    sync::mpsc::{self, Receiver},
    thread,
};
use url::Url;
use zeroize::Zeroizing;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Equipment,
    Live,
    Playback,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Brand {
    Intelbras,
    Hikvision,
    #[serde(alias = "Ezviz")]
    Onvif,
    Custom,
}

#[derive(Clone, Serialize, Deserialize)]
struct Equipment {
    #[serde(default)]
    id: String,
    #[serde(default)]
    disabled: bool,
    name: String,
    brand: Brand,
    host: String,
    port: u16,
    #[serde(default = "default_http_port")]
    http_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    utc_offset_minutes: Option<i64>,
    username: String,
    channels: u32,
}

fn default_http_port() -> u16 {
    80
}

fn format_utc_offset(minutes: i64) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let value = minutes.abs();
    format!("{sign}{:02}:{:02}", value / 60, value % 60)
}

fn device_playback_interval(
    brand: Brand,
    offset: Option<i64>,
    local: playback::Interval,
    utc: playback::Interval,
) -> Result<playback::Interval, &'static str> {
    match (brand, offset) {
        (Brand::Intelbras, Some(minutes)) => utc.shifted(minutes),
        (Brand::Intelbras, None) => Ok(local),
        _ => Ok(utc),
    }
}

type LocalConfig = config::Config<Vec<Equipment>>;

fn load_config() -> Result<LocalConfig, String> {
    let path = config::path().ok_or("Pasta de configuração indisponível.")?;
    let (mut config, legacy) = config::load_local::<Equipment>(&path)?;
    let import_passwords = config.passwords.is_none() && !config.equipment.is_empty();
    if import_passwords {
        let mut passwords = config::Passwords::new();
        for item in &config.equipment {
            if !item.id.is_empty() {
                if let Some(password) = config::legacy_password(&item.id)? {
                    passwords.insert(item.id.clone(), Zeroizing::new(password));
                }
            }
        }
        config.passwords = Some(passwords);
    }
    let changed = assign_missing_equipment_ids(&mut config.equipment);
    if legacy {
        config::migrate(&path, &config)
            .map_err(|_| "Não foi possível migrar as configurações para a pasta do executável.")?;
    } else if changed || import_passwords {
        config::save(&path, &config)
            .map_err(|_| "Não foi possível atualizar o cadastro existente.")?;
    }
    Ok(config)
}

fn assign_missing_equipment_ids(equipment: &mut [Equipment]) -> bool {
    let mut changed = false;
    for item in equipment {
        if item.id.is_empty() {
            item.id = uuid::Uuid::new_v4().to_string();
            changed = true;
        }
    }
    changed
}

fn validate_import(
    mut imported: config::Config<Vec<Equipment>>,
) -> Result<config::Config<Vec<Equipment>>, String> {
    if !(config::MIN_CACHE_SECONDS..=config::MAX_CACHE_SECONDS)
        .contains(&imported.playback_cache_seconds)
    {
        return Err("O tempo de cache do arquivo está fora do limite aceito.".into());
    }
    assign_missing_equipment_ids(&mut imported.equipment);
    let mut ids = HashSet::new();
    for (index, item) in imported.equipment.iter().enumerate() {
        if !ids.insert(&item.id) {
            return Err("O arquivo tem IDs de equipamentos duplicados.".into());
        }
        if item.name.trim().is_empty()
            || item.channels == 0
            || item.channels > 256
            || item.port == 0
            || item.http_port == 0
            || item
                .utc_offset_minutes
                .is_some_and(|offset| !(-840..=840).contains(&offset))
            || camera_url(Brand::Intelbras, &item.host, item.port, "", "", 1, false).is_err()
        {
            return Err(format!(
                "Os dados do equipamento {} estão inválidos.",
                index + 1
            ));
        }
    }
    if let Some(passwords) = &mut imported.passwords {
        passwords.retain(|id, _| {
            ids.contains(id)
                || id
                    .strip_suffix(":video")
                    .is_some_and(|id| ids.iter().any(|known| known.as_str() == id))
        });
    }
    Ok(imported)
}

fn save_config_at(
    path: &std::path::Path,
    equipment: &[Equipment],
    playback_cache_seconds: u16,
    passwords: &config::Passwords,
) -> Result<(), String> {
    config::save(
        path,
        &config_snapshot(equipment, playback_cache_seconds, passwords),
    )
}

fn config_snapshot<'a>(
    equipment: &'a [Equipment],
    playback_cache_seconds: u16,
    passwords: &config::Passwords,
) -> config::Config<&'a [Equipment]> {
    let mut settings = config::Config::new(equipment, playback_cache_seconds);
    settings.passwords = Some(passwords.clone());
    settings
}

fn index_after_swap(index: usize, first: usize, second: usize) -> usize {
    if index == first {
        second
    } else if index == second {
        first
    } else {
        index
    }
}

fn order_arrow(ui: &mut egui::Ui, enabled: bool, up: bool) -> egui::Response {
    let label = if up {
        "Mover equipamento para cima"
    } else {
        "Mover equipamento para baixo"
    };
    let (response, painter) = ui
        .add_enabled_ui(enabled, |ui| {
            ui.allocate_painter(
                egui::vec2(22.0, ui.spacing().interact_size.y),
                egui::Sense::click(),
            )
        })
        .inner;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let visuals = ui.style().interact(&response);
    painter.rect(
        response.rect,
        visuals.corner_radius,
        visuals.weak_bg_fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let center = response.rect.center();
    let dir = if up { -1.0 } else { 1.0 };
    let stroke = egui::Stroke::new(1.5_f32, visuals.fg_stroke.color);
    let tip = center + egui::vec2(0.0, dir * 4.5);
    painter.line_segment([center - egui::vec2(0.0, dir * 4.5), tip], stroke);
    painter.line_segment([center + egui::vec2(-4.0, 0.0), tip], stroke);
    painter.line_segment([tip, center + egui::vec2(4.0, 0.0)], stroke);
    response.on_hover_text(label)
}

fn camera_url(
    brand: Brand,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    channel: u32,
    secondary: bool,
) -> Result<Url, &'static str> {
    if host.is_empty() || host.trim() != host || host.contains(['/', '@', '?', '#']) {
        return Err("Informe somente o endereço IP ou nome do equipamento.");
    }
    if channel == 0 {
        return Err("O canal deve ser maior que zero.");
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let mut url = Url::parse(&format!("rtsp://{host}:{port}/"))
        .map_err(|_| "Endereço ou porta inválidos.")?;
    if !username.is_empty() {
        url.set_username(username)
            .map_err(|_| "Usuário inválido.")?;
        url.set_password(Some(password))
            .map_err(|_| "Senha inválida.")?;
    }
    match brand {
        Brand::Intelbras => {
            url.set_path("/cam/realmonitor");
            url.query_pairs_mut()
                .append_pair("channel", &channel.to_string())
                .append_pair("subtype", if secondary { "1" } else { "0" });
        }
        Brand::Hikvision => {
            url.set_path(&format!(
                "/Streaming/Channels/{channel}0{}",
                if secondary { 2 } else { 1 }
            ));
        }
        Brand::Onvif => {
            url.set_path("/ch1/main");
        }
        Brand::Custom => return Err("Informe uma URL RTSP personalizada."),
    }
    Ok(url)
}

fn onvif_device_url(rtsp: &Url, http_port: u16) -> Result<Url, &'static str> {
    let rest = rtsp
        .as_str()
        .strip_prefix("rtsp:")
        .ok_or("Endereço ONVIF inválido.")?;
    // URL's scheme setter cannot switch a non-special RTSP URL to special HTTP.
    let mut device = Url::parse(&format!("http:{rest}")).map_err(|_| "Endereço ONVIF inválido.")?;
    device
        .set_port(Some(http_port))
        .map_err(|_| "Porta ONVIF inválida.")?;
    device.set_path("/onvif/device_service");
    device.set_query(None);
    device
        .set_username("")
        .map_err(|_| "Endereço ONVIF inválido.")?;
    device
        .set_password(None)
        .map_err(|_| "Endereço ONVIF inválido.")?;
    Ok(device)
}

#[derive(Clone, Copy)]
struct ImageScale {
    width: f32,
    height: f32,
}

impl Default for ImageScale {
    fn default() -> Self {
        Self {
            width: 1.0,
            height: 1.0,
        }
    }
}

#[derive(Clone, Copy, Default)]
enum ImageFraming {
    #[default]
    Fill,
    Original,
    Custom(ImageScale),
}

fn original_image_size(rect: egui::Rect, pixels: [usize; 2]) -> egui::Vec2 {
    let aspect = pixels[0] as f32 / pixels[1] as f32;
    egui::vec2(
        rect.width().min(rect.height() * aspect),
        rect.height().min(rect.width() / aspect),
    )
}

impl ImageFraming {
    fn size(self, rect: egui::Rect, pixels: [usize; 2]) -> egui::Vec2 {
        match self {
            Self::Fill => rect.size(),
            Self::Original => original_image_size(rect, pixels),
            Self::Custom(scale) => {
                let fitted = original_image_size(rect, pixels);
                egui::vec2(fitted.x * scale.width, fitted.y * scale.height)
            }
        }
    }

    fn slider_values(self, rect: egui::Rect, pixels: Option<[usize; 2]>) -> ImageScale {
        match (self, pixels) {
            (Self::Fill, Some(pixels)) => {
                let fitted = original_image_size(rect, pixels);
                ImageScale {
                    width: rect.width() / fitted.x,
                    height: rect.height() / fitted.y,
                }
            }
            (Self::Custom(scale), _) => scale,
            _ => ImageScale::default(),
        }
    }
}

fn image_scale_menu(ui: &mut egui::Ui, slot: &mut VideoSlot, rect: egui::Rect) {
    ui.menu_button("Redimensionar imagem", |ui| {
        ui.label("Ajuste visual deste quadro");
        let mut scale = slot
            .framing
            .slider_values(rect, slot.texture.as_ref().map(|texture| texture.size()));
        let max_width = scale.width.max(4.0);
        let max_height = scale.height.max(4.0);
        let width = ui.add(
            egui::Slider::new(&mut scale.width, 0.25..=max_width)
                .text("Largura")
                .custom_formatter(|n, _| format!("{:.0}%", n * 100.0)),
        );
        let height = ui.add(
            egui::Slider::new(&mut scale.height, 0.25..=max_height)
                .text("Altura")
                .custom_formatter(|n, _| format!("{:.0}%", n * 100.0)),
        );
        if width.changed() || height.changed() {
            slot.framing = ImageFraming::Custom(scale);
        }
        if ui.button("Preencher quadro").clicked() {
            slot.framing = ImageFraming::Fill;
        }
        if ui.button("Restaurar proporção original").clicked() {
            slot.framing = ImageFraming::Original;
        }
        ui.small("Valores acima do quadro podem cortar a imagem.");
    });
}

fn paint_slot_border(ui: &egui::Ui, rect: egui::Rect, selected: bool) {
    let stroke = if selected {
        egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(35, 130, 255))
    } else {
        egui::Stroke::new(1.0_f32, egui::Color32::GRAY)
    };
    ui.painter()
        .rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
}

fn paint_playback_label(ui: &egui::Ui, rect: egui::Rect, text: &str, warning: bool) {
    let painter = ui.painter().with_clip_rect(rect);
    let color = if warning {
        egui::Color32::from_rgb(255, 192, 65)
    } else {
        egui::Color32::WHITE
    };
    let label = painter.layout(
        text.to_owned(),
        egui::FontId::proportional(13.0),
        color,
        (rect.width() - 16.0).max(1.0),
    );
    let pos = if warning {
        egui::pos2(
            rect.center().x - label.size().x / 2.0,
            rect.bottom() - label.size().y - 8.0,
        )
    } else {
        rect.min + egui::vec2(8.0, 6.0)
    };
    painter.rect_filled(
        egui::Rect::from_min_size(pos, label.size()).expand2(egui::vec2(5.0, 3.0)),
        3.0,
        egui::Color32::from_black_alpha(215),
    );
    painter.galley(pos, label, color);
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum LiveQuality {
    #[default]
    Auto,
    High,
    Low,
}

impl LiveQuality {
    const OPTIONS: [(Self, &'static str); 3] = [
        (Self::Auto, "Automática (pela grade)"),
        (Self::High, "Alta (fluxo principal)"),
        (Self::Low, "Baixa (fluxo secundário)"),
    ];

    fn use_secondary(self, side: usize, expanded: Option<usize>, index: usize) -> bool {
        match self {
            Self::Auto => side >= 3 && expanded != Some(index),
            Self::High => false,
            Self::Low => true,
        }
    }
}

#[derive(Default)]
struct VideoSlot {
    camera: Option<(String, u32)>,
    secondary: Option<bool>,
    live_quality: LiveQuality,
    frames: Option<Receiver<video::Event>>,
    errors: Option<Receiver<VideoMessage>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    texture: Option<egui::TextureHandle>,
    framing: ImageFraming,
    reconnecting: bool,
    playback: Option<playback::Progress>,
    playback_finished: Option<tokio::sync::oneshot::Receiver<()>>,
    playback_buffer: Option<std::sync::Arc<video::PlaybackBuffer>>,
    displayed_position: Option<f64>,
    displayed_at: Option<std::time::Instant>,
    playback_error: Option<&'static str>,
}

enum VideoMessage {
    Reconnecting(&'static str, u64),
    Failed(&'static str),
}

#[derive(Clone, Copy)]
struct PlaybackRequest {
    progress: playback::Progress,
    requested_utc: Option<jiff::civil::DateTime>,
    prefill_seconds: f64,
}

fn retry_delay(attempt: u32) -> u64 {
    (2_u64 << attempt.min(5)).min(60)
}

impl VideoSlot {
    fn has_recent_playback_frame(
        &self,
        now: std::time::Instant,
        position: f64,
        speed: playback::Speed,
    ) -> bool {
        self.displayed_position
            .zip(self.displayed_at)
            .is_some_and(|(time, shown)| {
                now.duration_since(shown) < playback::FRAME_STALL
                    && playback::frame_timing(time, position, speed)
                        == playback::FrameTiming::Present
            })
    }

    fn stop_worker(&mut self) {
        self.frames = None;
        self.errors = None;
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }

    fn stop(&mut self) {
        self.stop_worker();
        self.texture = None;
        self.camera = None;
        self.secondary = None;
        self.live_quality = LiveQuality::Auto;
        self.reconnecting = false;
        self.framing = ImageFraming::Fill;
        self.playback = None;
        self.playback_finished = None;
        self.playback_buffer = None;
        self.displayed_position = None;
        self.displayed_at = None;
        self.playback_error = None;
    }

    fn start(
        &mut self,
        url: Url,
        credentials: retina::client::Credentials,
        camera: (String, u32),
        ctx: &egui::Context,
        playback: Option<PlaybackRequest>,
    ) {
        let previous = self.playback_finished.take();
        self.stop();
        let reconnect = playback.is_none();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (error_sender, error_receiver) = mpsc::channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let ctx = ctx.clone();
        let playback_options = playback.map(|request| video::PlaybackOptions {
            rate: request.progress.speed.rate(),
            sessions: Default::default(),
            requested_utc: request.requested_utc,
            buffer: Default::default(),
            prefill_seconds: request.prefill_seconds,
            onvif_range: if url.scheme() == "http" {
                request.requested_utc.map(|start| {
                    let remaining = request.progress.interval.seconds() - request.progress.offset();
                    let end = start + jiff::SignedDuration::from_secs(remaining);
                    format!(
                        "clock={}-{}",
                        start.strftime("%Y%m%dT%H%M%SZ"),
                        end.strftime("%Y%m%dT%H%M%SZ")
                    )
                })
            } else {
                None
            },
        });
        self.playback_buffer = playback_options
            .as_ref()
            .map(|options| options.buffer.clone());
        thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ =
                    error_sender.send(VideoMessage::Failed("Não foi possível iniciar o vídeo."));
                ctx.request_repaint();
                return;
            };
            runtime.block_on(async {
                let mut cancel_rx = cancel_rx;
                if let Some(previous) = previous {
                    // Let the old playback session tear down before opening another on the DVR.
                    let _ = previous.await;
                }
                let mut attempt = 0;
                loop {
                    let started = std::time::Instant::now();
                    let result = tokio::select! {
                        biased;
                        _ = &mut cancel_rx => break,
                        result = async {
                            let stream = if url.scheme() == "http" {
                                if playback_options.is_some() {
                                    onvif::replay_url(&url, &credentials).await?
                                } else {
                                    onvif::stream_url(&url, &credentials).await?
                                }
                            } else {
                                url.clone()
                            };
                            video::run(stream, credentials.clone(), sender.clone(), Some(ctx.clone()), playback_options.clone(), url.scheme() == "http").await
                        } => result,
                    };
                    let error = match result {
                        Ok(()) => break,
                        Err(error) => error,
                    };
                    if !reconnect || !video::retryable(error) {
                        let _ = error_sender.send(VideoMessage::Failed(error));
                        ctx.request_repaint();
                        break;
                    }
                    if started.elapsed() >= std::time::Duration::from_secs(60) {
                        attempt = 0;
                    }
                    let delay = retry_delay(attempt);
                    attempt = attempt.saturating_add(1);
                    if error_sender.send(VideoMessage::Reconnecting(error, delay)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                    tokio::select! {
                        _ = &mut cancel_rx => break,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(delay)) => {}
                    };
                }
                if let Some(options) = playback_options {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        options.sessions.await_teardown(),
                    ).await;
                }
            });
            let _ = finished_tx.send(());
        });
        self.frames = Some(receiver);
        self.errors = Some(error_receiver);
        self.cancel = Some(cancel_tx);
        self.camera = Some(camera);
        self.playback = playback.map(|request| request.progress);
        if playback.is_some() {
            self.playback_finished = Some(finished_rx);
        }
    }
}

fn resize_grid(slots: &mut VideoSlots, selected: &mut usize, side: usize) {
    let count = side * side;
    for slot in &mut slots[count..] {
        slot.stop();
    }
    if *selected >= count {
        *selected = 0;
    }
}

fn next_slot(current: usize, side: usize) -> usize {
    (current + 1) % (side * side)
}

fn toggle_expanded(current: Option<usize>, index: usize) -> Option<usize> {
    if current == Some(index) {
        None
    } else {
        Some(index)
    }
}

fn grid_selector(ui: &mut egui::Ui, id: &str, current_side: usize, hint: &str) -> Option<usize> {
    let frame = egui::Frame::side_top_panel(ui.style());
    let spacing = ui.spacing();
    let height = spacing.interact_size.y
        + spacing.item_spacing.y
        + ui.text_style_height(&egui::TextStyle::Small)
        + frame.inner_margin.sum().y;
    egui::TopBottomPanel::bottom(ui.make_persistent_id(id))
        .frame(frame)
        .exact_height(height)
        .resizable(false)
        .show_inside(ui, |ui| {
            let mut selected = None;
            ui.horizontal(|ui| {
                for side in GRID_SIDES {
                    let count = side * side;
                    if ui
                        .selectable_label(current_side == side, count.to_string())
                        .on_hover_text(format!("{count} quadros"))
                        .clicked()
                    {
                        selected = Some(side);
                    }
                }
            });
            ui.small(hint);
            selected
        })
        .inner
}

fn datetime_field(ui: &mut egui::Ui, value: &mut String) {
    let before = value.clone();
    let mut output = egui::TextEdit::singleline(value)
        .hint_text("AAAA-MM-DD HH:MM:SS")
        .show(ui);
    if output.response.changed() {
        let range = output.state.cursor.char_range();
        let edited = value.clone();
        let cursor = range.map_or(edited.chars().count(), |r| r.primary.index);
        let mut digits: String = edited
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(14)
            .collect();
        let mut before_cursor = edited
            .chars()
            .take(cursor)
            .filter(|c| c.is_ascii_digit())
            .count()
            .min(digits.len());
        let old_digits: String = before.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits == old_digits && edited.chars().count() < before.chars().count() {
            let backspace = ui.input(|i| i.key_pressed(egui::Key::Backspace));
            let delete = ui.input(|i| i.key_pressed(egui::Key::Delete));
            if backspace && before_cursor > 0 {
                digits.remove(before_cursor - 1);
                before_cursor -= 1;
            } else if delete && before_cursor < digits.len() {
                digits.remove(before_cursor);
            }
        }
        *value = playback::mask_datetime(&digits);
        if let Some(range) = range {
            let position = |index: usize| {
                let count = edited
                    .chars()
                    .take(index)
                    .filter(|c| c.is_ascii_digit())
                    .count()
                    .min(digits.len());
                playback::cursor_after_digits(value, count)
            };
            let primary = playback::cursor_after_digits(value, before_cursor);
            let secondary = if range.primary.index == range.secondary.index {
                primary
            } else {
                position(range.secondary.index)
            };
            output
                .state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(primary),
                    egui::text::CCursor::new(secondary),
                )));
            output.state.store(ui.ctx(), output.response.id);
        }
    }
}

const MAX_GRID_SIDE: usize = 5;
const GRID_SIDES: std::ops::RangeInclusive<usize> = 2..=MAX_GRID_SIDE;
const MAX_GRID_SLOTS: usize = MAX_GRID_SIDE * MAX_GRID_SIDE;
type PlaybackCameras = [Option<(usize, u32)>; MAX_GRID_SLOTS];
type VideoSlots = [VideoSlot; MAX_GRID_SLOTS];

#[derive(Clone, Copy)]
struct PlaybackGroup {
    cameras: PlaybackCameras,
    interval: playback::Interval,
    clock: playback::SyncClock,
}

struct Viewer {
    tab: Tab,
    equipment_tab_open: bool,
    live_tab_open: bool,
    playback_tab_open: bool,
    edit_open: bool,
    about_open: bool,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    maintenance: Option<maintenance::Maintenance>,
    selected_slot: usize,
    slots: VideoSlots,
    grid_side: usize,
    expanded_slot: Option<usize>,
    quality_layout: (usize, Option<usize>),
    playback_slots: VideoSlots,
    playback_slot: usize,
    playback_grid_side: usize,
    playback_expanded_slot: Option<usize>,
    window_size: Option<egui::Vec2>,
    playback_cameras: PlaybackCameras,
    playback_group: Option<PlaybackGroup>,
    playback_start: String,
    playback_end: String,
    playback_speed: playback::Speed,
    playback_cache_seconds: u16,
    cache_draft: Option<u16>,
    pending_config_import: Option<LocalConfig>,
    pending_equipment_removal: Option<String>,
    export_result: Option<Receiver<(bool, &'static str)>>,
    export_notice: Option<String>,
    equipment: Vec<Equipment>,
    passwords: config::Passwords,
    load_failed: bool,
    selected: Option<usize>,
    name: String,
    channel_count: u32,
    brand: Brand,
    host: String,
    port: u16,
    username: String,
    password: String,
    channel: u32,
    custom_url: String,
    status: String,
    discovery: HashMap<String, (u64, discovery::State)>,
    discovery_tx: mpsc::Sender<(String, u64, discovery::State)>,
    discovery_rx: Receiver<(String, u64, discovery::State)>,
    discovery_started: bool,
    next_discovery: u64,
    http_port: u16,
    equipment_utc_offset: String,
}

impl Default for Viewer {
    fn default() -> Self {
        match load_config() {
            Ok(config) => Self::new(
                config,
                "Cadastre ou selecione um equipamento.".into(),
                false,
            ),
            Err(error) => Self::new(
                config::Config::new(Vec::new(), config::DEFAULT_CACHE_SECONDS),
                error,
                true,
            ),
        }
    }
}

impl Viewer {
    fn new(config: config::Config<Vec<Equipment>>, status: String, load_failed: bool) -> Self {
        let (discovery_tx, discovery_rx) = mpsc::channel();
        Self {
            tab: Tab::Live,
            equipment_tab_open: false,
            live_tab_open: true,
            playback_tab_open: false,
            edit_open: false,
            about_open: false,
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            maintenance: None,
            selected_slot: 0,
            slots: std::array::from_fn(|_| VideoSlot::default()),
            grid_side: 2,
            expanded_slot: None,
            quality_layout: (2, None),
            playback_slots: std::array::from_fn(|_| VideoSlot::default()),
            playback_slot: 0,
            playback_grid_side: 2,
            playback_expanded_slot: None,
            window_size: None,
            playback_cameras: [None; MAX_GRID_SLOTS],
            playback_group: None,
            playback_start: String::new(),
            playback_end: String::new(),
            playback_speed: playback::Speed::default(),
            playback_cache_seconds: config.playback_cache_seconds,
            cache_draft: None,
            pending_config_import: None,
            pending_equipment_removal: None,
            export_result: None,
            export_notice: None,
            equipment: config.equipment,
            passwords: config.passwords.unwrap_or_default(),
            load_failed,
            selected: None,
            name: String::new(),
            channel_count: 4,
            brand: Brand::Intelbras,
            host: String::new(),
            port: 554,
            username: String::new(),
            password: String::new(),
            channel: 1,
            custom_url: String::new(),
            status,
            discovery: HashMap::new(),
            discovery_tx,
            discovery_rx,
            discovery_started: false,
            next_discovery: 0,
            http_port: 80,
            equipment_utc_offset: String::new(),
        }
    }
}

impl Viewer {
    fn config_path(&self) -> Result<std::path::PathBuf, String> {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        if let Some(path) = self
            .maintenance
            .as_ref()
            .and_then(maintenance::Maintenance::moved_config)
        {
            return Ok(path.to_path_buf());
        }
        config::path().ok_or_else(|| "Pasta de configuração indisponível.".into())
    }

    fn config_writer(
        &self,
    ) -> impl FnOnce(&[Equipment], u16, &config::Passwords) -> Result<(), String> {
        let path = self.config_path();
        move |equipment, cache, passwords| save_config_at(&path?, equipment, cache, passwords)
    }

    fn equipment_password(&self, id: &str) -> Option<String> {
        self.passwords
            .get(id)
            .map(|password| password.as_str().to_owned())
    }

    fn prepare_import(&mut self, path: std::path::PathBuf) -> Result<(), String> {
        self.pending_config_import = Some(validate_import(config::read_import(&path)?)?);
        Ok(())
    }

    fn apply_config_import(
        &mut self,
        path: &std::path::Path,
        mut settings: LocalConfig,
    ) -> Result<(), String> {
        let passwords = settings.passwords.take().unwrap_or_default();
        config::backup_current(path)?;
        save_config_at(
            path,
            &settings.equipment,
            settings.playback_cache_seconds,
            &passwords,
        )?;

        self.stop();
        for slot in &mut self.playback_slots {
            slot.stop();
        }
        self.playback_cameras.fill(None);
        self.playback_group = None;
        self.playback_expanded_slot = None;
        self.expanded_slot = None;
        self.selected = None;
        self.pending_equipment_removal = None;
        self.edit_open = false;
        self.password.clear();
        self.equipment = settings.equipment;
        self.passwords = passwords;
        self.playback_cache_seconds = settings.playback_cache_seconds;
        self.load_failed = false;
        self.cache_draft = Some(self.playback_cache_seconds);
        // Imported addresses are not contacted just by importing the configuration.
        self.discovery_started = true;
        self.discovery = self
            .equipment
            .iter()
            .map(|item| {
                (
                    item.id.clone(),
                    (0, discovery::State::Unknown("Verificação pendente.")),
                )
            })
            .collect();
        Ok(())
    }

    fn backup_ui(&mut self, ctx: &egui::Context) {
        if let Some(imported) = self.pending_config_import.take() {
            let mut open = true;
            let mut confirm = false;
            let mut cancel = false;
            egui::Window::new("Confirmar importação")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "O arquivo contém {} equipamento(s) e {} senha(s).",
                        imported.equipment.len(),
                        imported
                            .passwords
                            .as_ref()
                            .map_or(0, |passwords| passwords.len())
                    ));
                    ui.label("Substituir a configuração atual e fechar as câmeras abertas?");
                    ui.small("O aplicativo guarda uma cópia local da configuração anterior.");
                    ui.horizontal(|ui| {
                        confirm = ui.button("Substituir configuração").clicked();
                        cancel = ui.button("Cancelar").clicked();
                    });
                });
            if confirm && open && !cancel {
                let result = self
                    .config_path()
                    .and_then(|path| self.apply_config_import(&path, imported));
                self.status = match result {
                    Ok(()) => {
                        "Configuração importada. A lista de equipamentos foi atualizada.".into()
                    }
                    Err(error) => error,
                };
            } else if open && !cancel {
                self.pending_config_import = Some(imported);
            }
        }
    }

    fn start_discovery(&mut self, equipment: Equipment, ctx: &egui::Context) {
        if equipment.disabled {
            return;
        }
        self.next_discovery += 1;
        let generation = self.next_discovery;
        self.discovery.insert(
            equipment.id.clone(),
            (generation, discovery::State::Checking),
        );
        let sender = self.discovery_tx.clone();
        let ctx = ctx.clone();
        let password = self.equipment_password(&equipment.id);
        thread::spawn(move || {
            let state = match password {
                Some(password) => discovery::discover(&equipment, password),
                None => discovery::State::Unknown("Senha não cadastrada."),
            };
            let _ = sender.send((equipment.id, generation, state));
            ctx.request_repaint();
        });
    }

    fn cameras_for(&self, equipment: &Equipment) -> Vec<discovery::Camera> {
        if let Some((
            _,
            discovery::State::Connected {
                cameras: Some(cameras),
            },
        )) = self.discovery.get(&equipment.id)
        {
            return cameras.clone();
        }
        (1..=equipment.channels)
            .map(|channel| discovery::Camera {
                channel,
                name: String::new(),
            })
            .collect()
    }

    fn device_title(&self, equipment: &Equipment) -> egui::RichText {
        let color = match self.discovery.get(&equipment.id).map(|(_, state)| state) {
            Some(discovery::State::Connected { .. }) => egui::Color32::from_rgb(61, 185, 104),
            Some(discovery::State::Failed(_)) => egui::Color32::from_rgb(232, 85, 85),
            _ => egui::Color32::GRAY,
        };
        egui::RichText::new(&equipment.name).color(color)
    }

    fn device_status(&self, equipment: &Equipment) -> &'static str {
        if equipment.disabled {
            return "Desativado.";
        }
        match self.discovery.get(&equipment.id).map(|(_, state)| state) {
            Some(discovery::State::Checking) | None => "Verificando…",
            Some(discovery::State::Connected { cameras: Some(_) }) => {
                "Conectado; canais obtidos do equipamento."
            }
            Some(discovery::State::Connected { cameras: None }) => {
                "Conectado; usando a quantidade de canais cadastrada."
            }
            Some(discovery::State::Failed(reason)) | Some(discovery::State::Unknown(reason)) => {
                reason
            }
        }
    }

    fn playback_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(220.0, ui.available_height()),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    if let Some(side) = grid_selector(
                        ui,
                        "playback_grid_selector",
                        self.playback_grid_side,
                        "Duplo clique para ampliar ou voltar.",
                    ) {
                        self.set_playback_grid_side(side);
                    }
                    ui.heading("Reprodução");
                    ui.label(format!(
                        "Escolha a câmera do quadro {}",
                        self.playback_slot + 1
                    ));
                    let list_height = (ui.available_height() - 315.0).max(36.0);
                    let mut playback_selection = None;
                    egui::ScrollArea::vertical()
                        .id_salt("playback_tree")
                        .max_height(list_height)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (index, item) in self
                                .equipment
                                .iter()
                                .enumerate()
                                .filter(|(_, item)| !item.disabled)
                            {
                                egui::CollapsingHeader::new(self.device_title(item))
                                    .id_salt(("playback_equipment", &item.id))
                                    .show(ui, |ui| {
                                        for camera in self.cameras_for(item) {
                                            let channel = camera.channel;
                                            if ui
                                                .selectable_label(
                                                    self.playback_cameras[self.playback_slot]
                                                        == Some((index, channel)),
                                                    discovery::label(&camera),
                                                )
                                                .clicked()
                                            {
                                                playback_selection = Some((index, channel));
                                            }
                                        }
                                    });
                            }
                        });
                    if let Some(camera) = playback_selection {
                        self.assign_playback_camera(camera);
                    }
                    ui.separator();
                    ui.label("Início (hora local do computador)");
                    datetime_field(ui, &mut self.playback_start);
                    ui.label("Fim (hora local do computador)");
                    datetime_field(ui, &mut self.playback_end);
                    let previous_speed = self.playback_speed;
                    ui.horizontal(|ui| {
                        ui.label("Velocidade");
                        egui::ComboBox::from_id_salt("playback_speed")
                            .selected_text(self.playback_speed.label())
                            .show_ui(ui, |ui| {
                                for speed in playback::Speed::ALL {
                                    ui.selectable_value(
                                        &mut self.playback_speed,
                                        speed,
                                        speed.label(),
                                    );
                                }
                            });
                    });
                    if self.playback_speed != previous_speed {
                        self.apply_playback_speed(ctx);
                    }
                    ui.small("A velocidade vale para todos os quadros em reprodução.");
                    if ui.button("Reproduzir em grupo").clicked() {
                        if let Err(error) = self.connect_playback(ctx) {
                            self.status = error.into();
                        }
                    }
                    if ui
                        .add_enabled(
                            self.export_result.is_none(),
                            egui::Button::new("Exportar intervalo…"),
                        )
                        .clicked()
                    {
                        if let Err(error) = self.start_export(ctx) {
                            self.status = error;
                        }
                    }
                    ui.small("Exporta só a câmera do quadro selecionado, em MKV sem áudio.");
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                let expanded = self.playback_expanded_slot;
                let side = if expanded.is_some() {
                    1
                } else {
                    self.playback_grid_side
                };
                let available = ui.available_size();
                let spacing = ui.spacing().item_spacing;
                let width = ((available.x - spacing.x * (side - 1) as f32) / side as f32).max(1.0);
                let height =
                    ((available.y - 88.0 - spacing.y * (side - 1) as f32) / side as f32).max(1.0);
                for row in 0..side {
                    ui.horizontal(|ui| {
                        for col in 0..side {
                            let index = expanded.unwrap_or(row * side + col);
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(width, height),
                                egui::Sense::click(),
                            );
                            if response.clicked() {
                                self.playback_slot = index;
                                ctx.request_repaint();
                            }
                            if response.double_clicked() {
                                self.toggle_playback_expansion(index);
                            }
                            response.context_menu(|ui| {
                                image_scale_menu(ui, &mut self.playback_slots[index], rect);
                                ui.separator();
                                if ui
                                    .add_enabled(
                                        self.playback_cameras[index].is_some(),
                                        egui::Button::new("Remover câmera do quadro"),
                                    )
                                    .clicked()
                                {
                                    self.playback_cameras[index] = None;
                                    self.remove_playback_member(index);
                                    self.status =
                                        format!("Reprodução do quadro {} encerrada.", index + 1);
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(
                                        self.playback_group.is_some(),
                                        egui::Button::new("Parar reprodução em grupo"),
                                    )
                                    .clicked()
                                {
                                    for slot in &mut self.playback_slots {
                                        slot.stop();
                                    }
                                    self.playback_group = None;
                                    self.status = "Reproduções encerradas.".into();
                                    ui.close_menu();
                                }
                            });
                            ui.painter().rect_filled(
                                rect,
                                0.0,
                                egui::Color32::from_rgb(20, 23, 26),
                            );
                            if let Some(texture) = &self.playback_slots[index].texture {
                                let size = self.playback_slots[index]
                                    .framing
                                    .size(rect, texture.size());
                                ui.painter().with_clip_rect(rect).image(
                                    texture.id(),
                                    egui::Rect::from_center_size(rect.center(), size),
                                    egui::Rect::from_min_max(
                                        egui::Pos2::ZERO,
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                            }
                            let assignment = self.playback_cameras[index];
                            let active = self.playback_group.and_then(|group| group.cameras[index]);
                            let label = (active.or(assignment))
                                .and_then(|(equipment, channel)| {
                                    self.equipment.get(equipment).map(|equipment| {
                                        format!(
                                            "Quadro {} · {} · câmera {channel}",
                                            index + 1,
                                            equipment.name
                                        )
                                    })
                                })
                                .unwrap_or_else(|| format!("Quadro {}", index + 1));
                            if assignment != active && active.is_some() {
                                paint_playback_label(
                                    ui,
                                    rect,
                                    &format!("{label} (seleção alterada)"),
                                    false,
                                );
                            } else {
                                paint_playback_label(ui, rect, &label, false);
                            }
                            if let Some(warning) = self.playback_warning(index) {
                                paint_playback_label(ui, rect, warning, true);
                            } else if self.playback_slots[index].texture.is_none() {
                                let text = if assignment.is_some() {
                                    "Aguardando reprodução"
                                } else {
                                    "Escolha uma câmera na lateral"
                                };
                                ui.painter().with_clip_rect(rect).text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    text,
                                    egui::FontId::proportional(13.0),
                                    egui::Color32::GRAY,
                                );
                            }
                            paint_slot_border(ui, rect, self.playback_slot == index);
                        }
                    });
                }
                ui.separator();
                self.playback_timeline(ui, ctx);
            });
        });
    }

    fn playback_timeline(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(group) = self.playback_group else {
            ui.label("Linha do tempo do grupo · Inicie a reprodução.");
            return;
        };
        let zone = "hora local do computador";
        let current =
            (group.clock.position(std::time::Instant::now()) as i64).min(group.interval.seconds());
        if let Some(offset) = playback_timeline_control(ui, group.interval, zone, current) {
            if let Err(error) = self.start_playback_group(group, offset, true, ctx) {
                self.status = error.into();
            }
        }
    }

    fn set_grid_side(&mut self, side: usize) {
        debug_assert!(GRID_SIDES.contains(&side));
        resize_grid(&mut self.slots, &mut self.selected_slot, side);
        self.grid_side = side;
        self.expanded_slot = None;
    }

    fn save(&mut self) {
        if self.load_failed {
            self.status =
                "Cadastro não carregado; salvamento bloqueado para preservar o arquivo existente."
                    .into();
            return;
        }
        if self.name.trim().is_empty() {
            self.status = "Informe um nome para o equipamento.".into();
            return;
        }
        let utc_offset_minutes = if self.equipment_utc_offset.trim().is_empty() {
            None
        } else {
            match playback::utc_offset_minutes(&self.equipment_utc_offset) {
                Ok(offset) => Some(offset),
                Err(error) => {
                    self.status = error.into();
                    return;
                }
            }
        };
        if let Err(error) = camera_url(
            self.brand,
            &self.host,
            self.port,
            &self.username,
            "",
            1,
            true,
        ) {
            self.status = error.into();
            return;
        }
        let equipment = Equipment {
            id: self
                .selected
                .map(|index| self.equipment[index].id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            disabled: self
                .selected
                .is_some_and(|index| self.equipment[index].disabled),
            name: self.name.trim().into(),
            brand: self.brand,
            host: self.host.clone(),
            port: self.port,
            http_port: self.http_port,
            utc_offset_minutes,
            username: self.username.clone(),
            channels: if self.brand == Brand::Onvif {
                1
            } else {
                self.channel_count
            },
        };
        let mut passwords = self.passwords.clone();
        if !self.password.is_empty() {
            passwords.insert(equipment.id.clone(), Zeroizing::new(self.password.clone()));
        }
        let mut updated = self.equipment.clone();
        if let Some(index) = self.selected {
            updated[index] = equipment;
        } else {
            updated.push(equipment);
        }
        match self.config_writer()(&updated, self.playback_cache_seconds, &passwords) {
            Ok(()) => {
                self.selected = Some(if self.selected.is_some() {
                    self.selected.unwrap()
                } else {
                    updated.len() - 1
                });
                self.equipment = updated;
                self.passwords = passwords;
                self.status = "Equipamento salvo.".into();
            }
            Err(error) => self.status = error,
        }
    }

    fn swap_equipment(
        &mut self,
        first: usize,
        second: usize,
        save: impl FnOnce(&[Equipment], u16, &config::Passwords) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.load_failed {
            return Err("Cadastro não carregado; a ordem não pode ser alterada.".into());
        }
        let mut updated = self.equipment.clone();
        updated.swap(first, second);
        save(&updated, self.playback_cache_seconds, &self.passwords)?;
        self.equipment = updated;

        // Playback and the edit form store list indices, while live sessions and
        // credentials already use stable equipment IDs.
        if let Some(index) = &mut self.selected {
            *index = index_after_swap(*index, first, second);
        }
        fn remap(cameras: &mut PlaybackCameras, first: usize, second: usize) {
            for (index, _) in cameras.iter_mut().flatten() {
                *index = index_after_swap(*index, first, second);
            }
        }
        remap(&mut self.playback_cameras, first, second);
        if let Some(group) = &mut self.playback_group {
            remap(&mut group.cameras, first, second);
        }
        Ok(())
    }

    fn set_equipment_enabled(
        &mut self,
        index: usize,
        enabled: bool,
        save: impl FnOnce(&[Equipment], u16, &config::Passwords) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.load_failed {
            return Err("Cadastro não carregado; alteração bloqueada.".into());
        }
        let equipment = self
            .equipment
            .get(index)
            .ok_or("Equipamento não encontrado.")?;
        if !enabled
            && (self.export_result.is_some()
                || self
                    .discovery
                    .get(&equipment.id)
                    .is_some_and(|(_, state)| matches!(state, discovery::State::Checking)))
        {
            return Err("Aguarde a verificação ou exportação terminar antes de desativar.".into());
        }
        let id = equipment.id.clone();
        let mut updated = self.equipment.clone();
        updated[index].disabled = !enabled;
        save(&updated, self.playback_cache_seconds, &self.passwords)?;
        self.equipment = updated;
        if !enabled {
            for slot in self.slots.iter_mut().chain(self.playback_slots.iter_mut()) {
                if slot
                    .camera
                    .as_ref()
                    .is_some_and(|(camera_id, _)| camera_id == &id)
                {
                    slot.stop();
                }
            }
            for camera in &mut self.playback_cameras {
                if camera.is_some_and(|(item, _)| item == index) {
                    *camera = None;
                }
            }
            if let Some(group) = &mut self.playback_group {
                for camera in &mut group.cameras {
                    if camera.is_some_and(|(item, _)| item == index) {
                        *camera = None;
                    }
                }
                if group.cameras.iter().all(Option::is_none) {
                    self.playback_group = None;
                }
            }
            self.discovery.remove(&id);
        }
        Ok(())
    }

    fn remove_equipment(
        &mut self,
        id: &str,
        save: impl FnOnce(&[Equipment], u16, &config::Passwords) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.load_failed {
            return Err("Cadastro não carregado; a remoção está bloqueada.".into());
        }
        let index = self
            .equipment
            .iter()
            .position(|item| item.id == id)
            .ok_or("Equipamento não encontrado.")?;
        let mut updated = self.equipment.clone();
        updated.remove(index);
        let mut passwords = self.passwords.clone();
        passwords.remove(id);
        passwords.remove(&format!("{id}:video"));
        save(&updated, self.playback_cache_seconds, &passwords)?;

        for slot in self.slots.iter_mut().chain(self.playback_slots.iter_mut()) {
            if slot
                .camera
                .as_ref()
                .is_some_and(|(camera_id, _)| camera_id == id)
            {
                slot.stop();
            }
        }
        fn remap(cameras: &mut PlaybackCameras, removed: usize) {
            for camera in cameras {
                if let Some((index, _)) = camera {
                    if *index == removed {
                        *camera = None;
                    } else if *index > removed {
                        *index -= 1;
                    }
                }
            }
        }
        remap(&mut self.playback_cameras, index);
        if let Some(group) = &mut self.playback_group {
            remap(&mut group.cameras, index);
            if group.cameras.iter().all(Option::is_none) {
                self.playback_group = None;
            }
        }
        if let Some(selected) = self.selected {
            if selected == index {
                self.selected = None;
                self.edit_open = false;
                self.password.clear();
            } else if selected > index {
                self.selected = Some(selected - 1);
            }
        }
        self.discovery.remove(id);
        self.equipment = updated;
        self.passwords = passwords;
        Ok(())
    }

    fn equipment_removal_ui(&mut self, ctx: &egui::Context) {
        let Some(id) = self.pending_equipment_removal.take() else {
            return;
        };
        let Some(equipment) = self.equipment.iter().find(|item| item.id == id) else {
            return;
        };
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Confirmar remoção")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Remover o equipamento \"{}\"?", equipment.name));
                ui.label("O cadastro e suas senhas serão removidos; câmeras abertas deste equipamento serão fechadas.");
                ui.horizontal(|ui| {
                    confirm = ui.button("Remover equipamento").clicked();
                    cancel = ui.button("Cancelar").clicked();
                });
            });
        if confirm && open && !cancel {
            let save = self.config_writer();
            self.status = match self.remove_equipment(&id, save) {
                Ok(()) => "Equipamento removido.".into(),
                Err(error) => error,
            };
        } else if open && !cancel {
            self.pending_equipment_removal = Some(id);
        }
    }

    fn select(&mut self, index: usize) {
        let equipment = &self.equipment[index];
        self.name = equipment.name.clone();
        self.brand = equipment.brand;
        self.host = equipment.host.clone();
        self.port = equipment.port;
        self.http_port = equipment.http_port;
        self.equipment_utc_offset = equipment
            .utc_offset_minutes
            .map(format_utc_offset)
            .unwrap_or_default();
        self.username = equipment.username.clone();
        self.channel_count = equipment.channels;
        self.password = self.equipment_password(&equipment.id).unwrap_or_default();
        self.selected = Some(index);
        self.channel = 1;
    }

    fn stop(&mut self) {
        for slot in &mut self.slots {
            slot.stop();
        }
    }

    fn connect(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let index = self.selected.ok_or("Selecione um equipamento.")?;
        if self.equipment.get(index).is_none_or(|item| item.disabled) {
            return Err("Equipamento desativado ou indisponível.".into());
        }
        let secondary =
            LiveQuality::Auto.use_secondary(self.grid_side, self.expanded_slot, self.selected_slot);
        let url = if self.brand == Brand::Custom {
            let url = Url::parse(&self.custom_url).map_err(|_| "URL inválida.")?;
            if url.scheme() != "rtsp" || url.host().is_none() {
                return Err("Informe uma URL rtsp:// válida.".into());
            }
            url
        } else {
            camera_url(
                self.brand,
                &self.host,
                self.port,
                &self.username,
                &self.password,
                self.channel,
                secondary,
            )?
        };
        let mut url = url;
        if self.brand == Brand::Onvif {
            url = onvif_device_url(&url, self.http_port)?;
        }
        let credentials = retina::client::Credentials {
            username: self.username.clone(),
            password: self.password.clone(),
        };
        url.set_username("").map_err(|_| "URL inválida.")?;
        url.set_password(None).map_err(|_| "URL inválida.")?;
        self.slots[self.selected_slot].start(
            url,
            credentials,
            (self.equipment[index].id.clone(), self.channel),
            ctx,
            None,
        );
        self.slots[self.selected_slot].secondary =
            (!matches!(self.brand, Brand::Custom | Brand::Onvif)).then_some(secondary);
        self.status = "Conectando…".into();
        if self.expanded_slot.is_none() {
            self.selected_slot = next_slot(self.selected_slot, self.grid_side);
        }
        Ok(())
    }

    fn set_live_quality(
        &mut self,
        index: usize,
        quality: LiveQuality,
        ctx: &egui::Context,
    ) -> Result<(), &'static str> {
        let desired = quality.use_secondary(self.grid_side, self.expanded_slot, index);
        let slot = &mut self.slots[index];
        let (Some(current), Some((id, channel))) = (slot.secondary, slot.camera.as_ref()) else {
            return Ok(());
        };

        if current != desired {
            let equipment = self
                .equipment
                .iter()
                .find(|item| &item.id == id)
                .ok_or("Não foi possível ajustar o fluxo: equipamento não encontrado.")?;
            if equipment.disabled {
                return Err("Equipamento desativado.");
            }
            let url = camera_url(
                equipment.brand,
                &equipment.host,
                equipment.port,
                "",
                "",
                *channel,
                desired,
            )?;
            let password = self
                .passwords
                .get(id)
                .map(|password| password.as_str().to_owned())
                .unwrap_or_default();
            let credentials = retina::client::Credentials {
                username: equipment.username.clone(),
                password,
            };
            let framing = slot.framing;
            slot.start(url, credentials, (id.clone(), *channel), ctx, None);
            slot.framing = framing;
            slot.secondary = Some(desired);
            self.status = format!("Ajustando a qualidade do quadro {}…", index + 1);
        }
        slot.live_quality = quality;
        Ok(())
    }

    fn sync_live_quality(&mut self, ctx: &egui::Context) {
        for index in 0..self.grid_side * self.grid_side {
            if let Err(error) = self.set_live_quality(index, self.slots[index].live_quality, ctx) {
                self.status = error.into();
            }
        }
    }

    fn open_playback_tab(&mut self, now: impl FnOnce() -> jiff::Zoned) {
        if !self.playback_tab_open {
            let times = playback::OpeningTimes::new(now());
            [self.playback_start, self.playback_end] = times.fields(false);
            self.playback_tab_open = true;
        }
        self.tab = Tab::Playback;
    }

    fn connect_playback(&mut self, ctx: &egui::Context) -> Result<(), &'static str> {
        let cameras = self.visible_playback_cameras();
        if cameras.iter().all(Option::is_none) {
            return Err("Escolha uma câmera em pelo menos um quadro.");
        }
        let interval = playback::Interval::new(&self.playback_start, &self.playback_end)?;
        interval.local_to_utc(&jiff::tz::TimeZone::system())?;
        let group = PlaybackGroup {
            cameras,
            interval,
            clock: playback::SyncClock::new(
                0,
                self.playback_speed,
                f64::from(self.playback_cache_seconds),
                std::time::Instant::now(),
            ),
        };
        self.start_playback_group(group, 0, false, ctx)
    }

    fn visible_playback_cameras(&self) -> PlaybackCameras {
        let mut cameras = self.playback_cameras;
        cameras[self.playback_grid_side.pow(2)..].fill(None);
        cameras
    }

    fn assign_playback_camera(&mut self, camera: (usize, u32)) {
        self.playback_cameras[self.playback_slot] = Some(camera);
        if self.playback_expanded_slot.is_none() {
            self.playback_slot = next_slot(self.playback_slot, self.playback_grid_side);
        }
    }

    fn toggle_playback_expansion(&mut self, index: usize) {
        self.playback_slot = index;
        self.playback_expanded_slot = toggle_expanded(self.playback_expanded_slot, index);
    }

    fn set_playback_grid_side(&mut self, side: usize) {
        debug_assert!(GRID_SIDES.contains(&side));
        self.playback_expanded_slot = None;
        if side < self.playback_grid_side {
            let count = side * side;
            for index in count..self.playback_grid_side.pow(2) {
                self.remove_playback_member(index);
            }
            if self.playback_slot >= count {
                self.playback_slot = 0;
            }
        }
        self.playback_grid_side = side;
    }

    fn start_playback_group(
        &mut self,
        mut group: PlaybackGroup,
        offset: i64,
        preserve_framing: bool,
        ctx: &egui::Context,
    ) -> Result<(), &'static str> {
        let offset = group.interval.seek_offset(offset);
        let prefill_seconds = playback::Progress::new(group.interval, offset, self.playback_speed)
            .prefill_seconds(self.playback_cache_seconds);
        let utc_interval = group.interval.local_to_utc(&jiff::tz::TimeZone::system())?;
        let requested_utc = Some(utc_interval.at(offset));
        let mut opened = 0;
        let mut failures = 0;
        for (slot, camera) in self.playback_slots.iter_mut().zip(group.cameras) {
            let Some((index, channel)) = camera else {
                slot.stop();
                continue;
            };
            let equipment = self.equipment.get(index);
            let same_camera = slot.camera.as_ref().zip(equipment).is_some_and(
                |((id, old_channel), equipment)| *id == equipment.id && *old_channel == channel,
            );
            let last_image = same_camera.then(|| slot.texture.clone()).flatten();
            let framing = slot.framing;
            let result = equipment
                .ok_or("Equipamento indisponível.")
                .and_then(|equipment| {
                    if equipment.disabled {
                        return Err("Equipamento desativado.");
                    }
                    let interval = device_playback_interval(
                        equipment.brand,
                        equipment.utc_offset_minutes,
                        group.interval,
                        utc_interval,
                    )?;
                    let progress = playback::Progress::new(interval, offset, self.playback_speed);
                    let request = PlaybackRequest {
                        progress,
                        requested_utc,
                        prefill_seconds,
                    };
                    let password = self
                        .passwords
                        .get(&equipment.id)
                        .ok_or("Senha não cadastrada.")?;
                    start_recording(slot, equipment, channel, request, password, ctx)
                });
            match result {
                Ok(()) => opened += 1,
                Err(error) => {
                    slot.stop();
                    slot.playback_error = Some(error);
                    failures += 1;
                }
            }
            slot.texture = last_image;
            if preserve_framing && same_camera {
                slot.framing = framing;
            }
        }
        group.clock = playback::SyncClock::new(
            offset,
            self.playback_speed,
            prefill_seconds,
            std::time::Instant::now(),
        );
        self.playback_group = Some(group);
        self.status = if failures > 0 {
            format!(
                "Reprodução em grupo: iniciando {opened} quadro(s); {failures} indisponível(is)."
            )
        } else {
            format!(
                "Iniciando reprodução de {opened} quadro(s) a {}…",
                self.playback_speed.label()
            )
        };
        Ok(())
    }

    fn apply_playback_speed(&mut self, ctx: &egui::Context) {
        if let Some(group) = self.playback_group {
            let offset = group.clock.position(std::time::Instant::now()) as i64;
            if offset < group.interval.seconds() {
                if let Err(error) = self.start_playback_group(group, offset, true, ctx) {
                    self.status = error.into();
                }
            }
        }
    }

    fn remove_playback_member(&mut self, index: usize) {
        self.playback_slots[index].stop();
        if let Some(group) = &mut self.playback_group {
            group.cameras[index] = None;
            if group.cameras.iter().all(Option::is_none) {
                self.playback_group = None;
            }
        }
    }

    fn playback_warning(&self, index: usize) -> Option<&'static str> {
        let group = self.playback_group?;
        group.cameras[index]?;
        let slot = &self.playback_slots[index];
        let now = std::time::Instant::now();
        let position = group.clock.position(now);
        if position >= group.interval.seconds() as f64 {
            Some("Fim do intervalo")
        } else if !group.clock.started() {
            Some(slot.playback_error.unwrap_or("Preparando vídeo…"))
        } else if !slot.has_recent_playback_frame(now, position, group.clock.speed()) {
            Some(slot.playback_error.unwrap_or("Sem vídeo neste horário"))
        } else {
            None
        }
    }

    fn start_export(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let (index, channel) = self.playback_cameras[self.playback_slot]
            .ok_or("Selecione um quadro com câmera para exportar.")?;
        let equipment = self
            .equipment
            .get(index)
            .ok_or("Equipamento indisponível.")?
            .clone();
        if equipment.disabled {
            return Err("Equipamento desativado.".into());
        }
        if matches!(equipment.brand, Brand::Custom | Brand::Onvif) {
            return Err("Exportação não disponível para este tipo de equipamento.".into());
        }
        let base = camera_url(
            equipment.brand,
            &equipment.host,
            equipment.port,
            "",
            "",
            channel,
            false,
        )?;
        let local = playback::Interval::new(&self.playback_start, &self.playback_end)?;
        let utc = local.local_to_utc(&jiff::tz::TimeZone::system())?;
        let interval =
            device_playback_interval(equipment.brand, equipment.utc_offset_minutes, local, utc)?;
        let url = interval.url_at(base, equipment.brand, channel, 0)?;
        let password = self
            .equipment_password(&equipment.id)
            .ok_or("Senha não cadastrada.")?;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.export_result = Some(rx);
        self.status = "Escolha onde salvar a exportação…".into();
        thread::spawn(move || {
            let result = match rfd::FileDialog::new()
                .set_title("Exportar vídeo")
                .add_filter("Vídeo Matroska", &["mkv"])
                .set_file_name("exportacao.mkv")
                .save_file()
            {
                None => "Exportação cancelada.",
                Some(path) => {
                    let _ = tx.send((
                        false,
                        "Exportando vídeo em segundo plano… mantenha o aplicativo aberto.",
                    ));
                    ctx.request_repaint();
                    let credentials = retina::client::Credentials {
                        username: equipment.username,
                        password,
                    };
                    match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => {
                            match runtime.block_on(export::run(url, credentials, &path)) {
                                Ok(message) | Err(message) => message,
                            }
                        }
                        Err(_) => "Não foi possível iniciar a exportação.",
                    }
                }
            };
            let _ = tx.send((true, result));
            ctx.request_repaint();
        });
        Ok(())
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop();
        for slot in &mut self.playback_slots {
            slot.stop();
        }
    }
}

fn start_recording(
    slot: &mut VideoSlot,
    equipment: &Equipment,
    channel: u32,
    request: PlaybackRequest,
    password: &str,
    ctx: &egui::Context,
) -> Result<(), &'static str> {
    let progress = request.progress;
    if equipment.brand == Brand::Custom {
        return Err("Reprodução não disponível para este tipo de equipamento.");
    }
    let base = camera_url(
        equipment.brand,
        &equipment.host,
        equipment.port,
        "",
        "",
        channel,
        true,
    )?;
    let url = if equipment.brand == Brand::Onvif {
        onvif_device_url(&base, equipment.http_port)?
    } else {
        progress
            .interval
            .url_at(base, equipment.brand, channel, progress.offset())?
    };
    slot.start(
        url,
        retina::client::Credentials {
            username: equipment.username.clone(),
            password: password.to_owned(),
        },
        (equipment.id.clone(), channel),
        ctx,
        Some(request),
    );
    Ok(())
}

fn playback_timeline_control(
    ui: &mut egui::Ui,
    interval: playback::Interval,
    zone: &str,
    current: i64,
) -> Option<i64> {
    ui.label(format!("Linha do tempo do grupo ({zone})"));
    let ctx = ui.ctx().clone();
    let id = ui.id().with("playback_group_timeline");
    let mut offset = ctx.data(|data| data.get_temp::<i64>(id)).unwrap_or(current);
    let response = ui
        .scope(|ui| {
            ui.spacing_mut().slider_width = ui.available_width();
            ui.push_id(id, |ui| {
                ui.add(
                    egui::Slider::new(&mut offset, 0..=interval.seconds())
                        .show_value(false)
                        .smart_aim(false),
                )
            })
            .inner
        })
        .inner;
    let target = interval.seek_offset(offset);
    let seek = if response.dragged() {
        ctx.data_mut(|data| data.insert_temp(id, offset));
        if let Some(pointer) = response.interact_pointer_pos() {
            let slider = response.rect;
            let x = pointer.x.clamp(slider.left(), slider.right());
            paint_timeline_preview(
                ui,
                id,
                egui::pos2(x, slider.top() - 6.0),
                &format!("Ir para {} ({zone})", interval.label_at(target),),
            );
        }
        None
    } else {
        ctx.data_mut(|data| data.remove::<i64>(id));
        ((response.drag_stopped() || response.changed()) && target != current).then_some(target)
    };
    let position = if response.dragged() {
        target
    } else {
        seek.unwrap_or(offset)
    };
    ui.small(format!(
        "Posição aprox.: {}  |  Intervalo: {} a {}",
        interval.label_at(position),
        interval.label_at(0),
        interval.label_at(interval.seconds()),
    ));
    ui.small("A barra não indica quais horários têm gravação.");
    seek
}

fn paint_timeline_preview(ui: &egui::Ui, id: egui::Id, anchor: egui::Pos2, text: &str) {
    let ctx = ui.ctx();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, id));
    let visuals = ui.visuals();
    let text = painter.layout_no_wrap(
        text.to_owned(),
        egui::TextStyle::Body.resolve(ui.style()),
        visuals.text_color(),
    );
    let padding = egui::vec2(8.0, 5.0);
    let size = text.size() + 2.0 * padding;
    let screen = ctx.screen_rect().shrink(2.0);
    let min = (anchor - egui::vec2(size.x / 2.0, size.y))
        .min(screen.max - size)
        .max(screen.min);
    let rect = egui::Rect::from_min_size(min, size);
    painter.rect(
        rect,
        5.0,
        visuals.window_fill(),
        visuals.window_stroke(),
        egui::StrokeKind::Inside,
    );
    painter.galley(min + padding, text, visuals.text_color());
}

fn present_frame(
    slot: &mut VideoSlot,
    frame: video::Frame,
    ctx: &egui::Context,
    label: &str,
    index: usize,
) {
    slot.reconnecting = false;
    if let (Some(progress), Some(elapsed)) = (&mut slot.playback, frame.recording_elapsed) {
        slot.displayed_position = Some(progress.frame_position(elapsed));
        progress.update(elapsed);
    }
    let image = egui::ColorImage::from_rgb([frame.width, frame.height], &frame.rgb);
    if let Some(texture) = &mut slot.texture {
        texture.set(image, egui::TextureOptions::LINEAR);
    } else {
        slot.texture = Some(ctx.load_texture(
            format!("{label}-{index}"),
            image,
            egui::TextureOptions::LINEAR,
        ));
    }
}

fn update_slots(slots: &mut [VideoSlot], ctx: &egui::Context, status: &mut String, label: &str) {
    for (index, slot) in slots.iter_mut().enumerate() {
        if let Some(event) = slot
            .frames
            .as_ref()
            .and_then(|frames| frames.try_recv().ok())
        {
            match event {
                video::Event::Frame(frame) => present_frame(slot, frame, ctx, label, index),
                video::Event::Status(message) => {
                    *status = format!("{label}, quadro {}: {message}", index + 1)
                }
            }
        }
        if let Some(message) = slot
            .errors
            .as_ref()
            .and_then(|errors| errors.try_recv().ok())
        {
            match message {
                VideoMessage::Reconnecting(reason, delay) => {
                    slot.reconnecting = true;
                    slot.texture = None;
                    *status = format!(
                        "{label}, quadro {}: {reason} Nova tentativa em {delay} segundos.",
                        index + 1
                    );
                }
                VideoMessage::Failed(reason) => {
                    *status = format!("{label}, quadro {}: {reason}", index + 1);
                    if slot.playback.is_some() {
                        slot.stop_worker();
                        if reason != video::STREAM_ENDED {
                            slot.playback_error = Some(reason);
                        }
                    } else {
                        slot.stop();
                    }
                }
            }
        }
    }
}

fn update_group(
    group: &mut PlaybackGroup,
    slots: &mut VideoSlots,
    ctx: &egui::Context,
    now: std::time::Instant,
) {
    use std::time::Duration;

    let speed = group.clock.speed();
    let rate = speed.rate();
    if !group.clock.started() {
        let ready = slots.iter().zip(group.cameras).all(|(slot, camera)| {
            camera.is_none()
                || slot.cancel.is_none()
                || slot
                    .playback_buffer
                    .as_ref()
                    .is_some_and(|buffer| buffer.ready())
        });
        group.clock.tick(now, ready);
    }
    let position = group.clock.position(now);
    if position >= group.interval.seconds() as f64 {
        for slot in slots {
            slot.stop_worker();
            slot.playback_buffer = None;
        }
        return;
    }
    // Wake even when there is no video so the startup deadline and warnings advance.
    let mut repaint_after = Duration::from_millis(100);
    if !group.clock.started() {
        ctx.request_repaint_after(repaint_after);
        return;
    }
    for (index, slot) in slots.iter_mut().enumerate() {
        let (Some(progress), Some(buffer)) = (slot.playback, &slot.playback_buffer) else {
            continue;
        };
        let due = playback::due_through(position, speed);
        let (frame, next) = buffer.take_due(due - progress.frame_position(0.0));
        if let Some(next) = next {
            let delay = ((progress.frame_position(next) - due) / rate).clamp(0.001, 0.1);
            repaint_after = repaint_after.min(Duration::from_secs_f64(delay));
        }
        let Some(frame) = frame else {
            continue;
        };
        let Some(elapsed) = frame.recording_elapsed else {
            continue;
        };
        let timestamp = progress.frame_position(elapsed);
        if timestamp >= 0.0
            && playback::frame_timing(timestamp, position, speed) != playback::FrameTiming::Drop
            && slot.displayed_position.is_none_or(|last| timestamp >= last)
        {
            if slot.displayed_position != Some(timestamp) {
                slot.displayed_at = Some(now);
            }
            present_frame(slot, frame, ctx, "Reprodução", index);
        }
    }
    ctx.request_repaint_after(repaint_after);
}

fn poll_export(
    receiver: &mut Option<Receiver<(bool, &'static str)>>,
    status: &mut String,
) -> Option<String> {
    let rx = receiver.as_ref()?;
    loop {
        match rx.try_recv() {
            Ok((false, message)) => *status = message.into(),
            Ok((true, message)) => {
                *status = message.into();
                *receiver = None;
                return Some(message.into());
            }
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                let message = "A exportação foi interrompida inesperadamente; o arquivo pode estar incompleto.";
                *status = message.into();
                *receiver = None;
                return Some(message.into());
            }
        }
    }
}

fn reset_framing_after_resize(
    previous_size: &mut Option<egui::Vec2>,
    size: egui::Vec2,
    live: &mut [VideoSlot],
    playback: &mut [VideoSlot],
) {
    if previous_size
        .replace(size)
        .is_some_and(|previous| previous != size)
    {
        for slot in live.iter_mut().chain(playback) {
            slot.framing = ImageFraming::Fill;
        }
    }
}

impl eframe::App for Viewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        reset_framing_after_resize(
            &mut self.window_size,
            ctx.screen_rect().size(),
            &mut self.slots,
            &mut self.playback_slots,
        );
        if let Some(message) = poll_export(&mut self.export_result, &mut self.status) {
            self.export_notice = Some(message);
        }
        if self.export_result.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
        if !self.discovery_started {
            self.discovery_started = true;
            for equipment in self.equipment.clone() {
                self.start_discovery(equipment, ctx);
            }
        }
        while let Ok((id, generation, state)) = self.discovery_rx.try_recv() {
            if self
                .discovery
                .get(&id)
                .is_some_and(|(current, _)| *current == generation)
            {
                self.discovery.insert(id, (generation, state));
            }
        }
        update_slots(&mut self.slots, ctx, &mut self.status, "Ao vivo");
        update_slots(
            &mut self.playback_slots,
            ctx,
            &mut self.status,
            "Reprodução",
        );
        if let Some(group) = &mut self.playback_group {
            update_group(
                group,
                &mut self.playback_slots,
                ctx,
                std::time::Instant::now(),
            );
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("☰ Menu", |ui| {
                    if ui.button("Visualização ao vivo").clicked() {
                        self.live_tab_open = true;
                        self.tab = Tab::Live;
                        ui.close_menu();
                    }
                    if ui.button("Reprodução").clicked() {
                        self.open_playback_tab(jiff::Zoned::now);
                        ui.close_menu();
                    }
                    if ui.button("Equipamentos").clicked() {
                        self.equipment_tab_open = true;
                        self.tab = Tab::Equipment;
                        ui.close_menu();
                    }
                    if ui.button("Configuração").clicked() {
                        self.cache_draft = Some(self.playback_cache_seconds);
                        ui.close_menu();
                    }
                    if ui.button("Sobre").clicked() {
                        self.about_open = true;
                        ui.close_menu();
                    }
                });
                ui.separator();
                if self.equipment_tab_open {
                    if ui
                        .selectable_label(self.tab == Tab::Equipment, "Equipamentos")
                        .clicked()
                    {
                        self.tab = Tab::Equipment;
                    }
                    if ui
                        .small_button("×")
                        .on_hover_text("Fechar aba de equipamentos")
                        .clicked()
                    {
                        self.equipment_tab_open = false;
                        if self.tab == Tab::Equipment {
                            if self.live_tab_open {
                                self.tab = Tab::Live;
                            } else if self.playback_tab_open {
                                self.tab = Tab::Playback;
                            }
                        }
                    }
                }
                if self.live_tab_open {
                    if ui
                        .selectable_label(self.tab == Tab::Live, "Visualização ao vivo")
                        .clicked()
                    {
                        self.tab = Tab::Live;
                    }
                    if ui
                        .small_button("×")
                        .on_hover_text("Fechar aba de visualização")
                        .clicked()
                    {
                        self.live_tab_open = false;
                        self.stop();
                        self.expanded_slot = None;
                        if self.tab == Tab::Live {
                            if self.playback_tab_open {
                                self.tab = Tab::Playback;
                            } else if self.equipment_tab_open {
                                self.tab = Tab::Equipment;
                            }
                        }
                    }
                }
                if self.playback_tab_open {
                    if ui
                        .selectable_label(self.tab == Tab::Playback, "Reprodução")
                        .clicked()
                    {
                        self.tab = Tab::Playback;
                    }
                    if ui
                        .small_button("×")
                        .on_hover_text("Fechar aba de reprodução")
                        .clicked()
                    {
                        self.playback_tab_open = false;
                        self.playback_expanded_slot = None;
                        for slot in &mut self.playback_slots {
                            slot.stop();
                        }
                        self.playback_group = None;
                        if self.tab == Tab::Playback {
                            if self.live_tab_open {
                                self.tab = Tab::Live;
                            } else if self.equipment_tab_open {
                                self.tab = Tab::Equipment;
                            }
                        }
                    }
                }
            });
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.label(&self.status);
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.equipment_tab_open && !self.live_tab_open && !self.playback_tab_open {
                ui.heading("RustDVR");
                ui.label("Abra uma aba pelo menu.");
            } else if self.tab == Tab::Equipment {
                ui.heading("Equipamentos");
                ui.horizontal(|ui| {
                    if ui.button("＋ Adicionar").clicked() {
                        self.selected = None;
                        self.name.clear();
                        self.host.clear();
                        self.username.clear();
                        self.password.clear();
                        self.brand = Brand::Intelbras;
                        self.port = 554;
                        self.http_port = 80;
                        self.equipment_utc_offset.clear();
                        self.channel_count = 4;
                        self.edit_open = true;
                    }
                    ui.label(format!(
                        "Equipamentos cadastrados: {}",
                        self.equipment.len()
                    ));
                    let checking = self
                        .discovery
                        .values()
                        .any(|(_, state)| matches!(state, discovery::State::Checking));
                    if ui
                        .add_enabled(!checking, egui::Button::new("Verificar novamente"))
                        .clicked()
                    {
                        for equipment in self.equipment.clone() {
                            self.start_discovery(equipment, ctx);
                        }
                    }
                });
                ui.separator();
                let mut edit = None;
                let mut reorder = None;
                let mut remove = None;
                let mut enabled_change = None;
                egui::ScrollArea::both().show(ui, |ui| {
                    egui::Grid::new("equipment_table")
                        .striped(true)
                        .min_col_width(100.0)
                        .show(ui, |ui| {
                            ui.strong("Nome");
                            ui.strong("IP / domínio");
                            ui.strong("Marca");
                            ui.strong("Porta RTSP");
                            ui.strong("Canais");
                            ui.strong("Ativo");
                            ui.strong("Ordem");
                            ui.strong("Ação");
                            ui.end_row();
                            for (index, equipment) in self.equipment.iter().enumerate() {
                                ui.label(self.device_title(equipment))
                                    .on_hover_text(self.device_status(equipment));
                                ui.label(&equipment.host);
                                ui.label(match equipment.brand {
                                    Brand::Intelbras => "Intelbras",
                                    Brand::Hikvision => "Hikvision",
                                    Brand::Onvif => "ONVIF",
                                    Brand::Custom => "Personalizado",
                                });
                                ui.label(if equipment.brand == Brand::Onvif {
                                    "Automática".to_owned()
                                } else {
                                    equipment.port.to_string()
                                });
                                let count = self.cameras_for(equipment).len();
                                ui.label(count.to_string())
                                    .on_hover_text(self.device_status(equipment));
                                let mut enabled = !equipment.disabled;
                                if ui.add_enabled(!self.load_failed, egui::Checkbox::without_text(&mut enabled))
                                    .on_hover_text("Desative para desconectar e ocultar nas listas de câmeras.")
                                    .changed()
                                {
                                    enabled_change = Some((index, enabled));
                                }
                                ui.push_id(&equipment.id, |ui| {
                                    ui.horizontal(|ui| {
                                        if order_arrow(ui, !self.load_failed && index > 0, true)
                                            .clicked()
                                        {
                                            reorder = Some((index, index - 1));
                                        }
                                        if order_arrow(
                                            ui,
                                            !self.load_failed && index + 1 < self.equipment.len(),
                                            false,
                                        )
                                        .clicked()
                                        {
                                            reorder = Some((index, index + 1));
                                        }
                                    });
                                });
                                ui.horizontal(|ui| {
                                    if ui.button("Editar").clicked() {
                                        edit = Some(index);
                                    }
                                    if ui
                                        .add_enabled(
                                            !self.load_failed,
                                            egui::Button::new("Remover"),
                                        )
                                        .clicked()
                                    {
                                        remove = Some(equipment.id.clone());
                                    }
                                });
                                ui.end_row();
                            }
                        });
                });
                if let Some(index) = edit {
                    self.select(index);
                    self.edit_open = true;
                }
                if let Some((index, enabled)) = enabled_change {
                    let save = self.config_writer();
                    self.status = match self.set_equipment_enabled(index, enabled, save) {
                        Ok(()) => {
                            if enabled {
                                self.start_discovery(self.equipment[index].clone(), ctx);
                                "Equipamento ativado.".into()
                            } else {
                                "Equipamento desativado e desconectado.".into()
                            }
                        }
                        Err(error) => error,
                    };
                }
                if let Some(id) = remove {
                    self.pending_equipment_removal = Some(id);
                }
                if let Some((first, second)) = reorder {
                    let save = self.config_writer();
                    self.status = match self.swap_equipment(first, second, save) {
                        Ok(()) => "Ordem dos equipamentos salva.".into(),
                        Err(error) => error,
                    };
                }
            } else if self.tab == Tab::Playback {
                self.playback_ui(ui, ctx);
            } else {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(205.0, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            if let Some(side) = grid_selector(
                                ui,
                                "live_grid_selector",
                                self.grid_side,
                                "Qualidade ajustada à grade.",
                            ) {
                                self.set_grid_side(side);
                            }
                            ui.heading("Câmeras");
                            ui.separator();
                            let mut connect_camera = None;
                            let mut disconnect_camera = None;
                            let mut disconnect_all = false;
                            let list_height = ui.available_height();
                            egui::ScrollArea::vertical()
                                .max_height(list_height)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for (index, item) in self.equipment.iter().enumerate().filter(|(_, item)| !item.disabled) {
                                        egui::CollapsingHeader::new(self.device_title(item))
                                            .id_salt(("camera_tree", &item.id))
                                            .show(ui, |ui| {
                                                ui.small(self.device_status(item));
                                                for camera in self.cameras_for(item) {
                                                    let channel = camera.channel;
                                                    let active = self.slots.iter().any(|slot| {
                                                        slot.camera.as_ref().is_some_and(
                                                            |(id, current)| {
                                                                id == &item.id
                                                                    && *current == channel
                                                            },
                                                        )
                                                    });
                                                    let response = ui.selectable_label(
                                                        active,
                                                        discovery::label(&camera),
                                                    );
                                                    if response.double_clicked() {
                                                        connect_camera = Some((index, channel));
                                                    }
                                                    response.context_menu(|ui| {
                                                        if ui
                                                            .add_enabled(
                                                                active,
                                                                egui::Button::new("Desconectar"),
                                                            )
                                                            .clicked()
                                                        {
                                                            disconnect_camera =
                                                                Some((item.id.clone(), channel));
                                                            ui.close_menu();
                                                        }
                                                        if ui
                                                            .add_enabled(
                                                                self.slots.iter().any(|slot| {
                                                                    slot.camera.is_some()
                                                                }),
                                                                egui::Button::new(
                                                                    "Fechar todas as câmeras",
                                                                ),
                                                            )
                                                            .clicked()
                                                        {
                                                            disconnect_all = true;
                                                            ui.close_menu();
                                                        }
                                                    });
                                                }
                                            });
                                    }
                                });
                            if let Some((index, channel)) = connect_camera {
                                self.select(index);
                                self.channel = channel;
                                if let Err(error) = self.connect(ctx) {
                                    self.status = error;
                                }
                            }
                            if let Some(camera) = disconnect_camera {
                                for slot in &mut self.slots {
                                    if slot.camera.as_ref() == Some(&camera) {
                                        slot.stop();
                                    }
                                }
                                self.status = "Desconectado.".into();
                            }
                            if disconnect_all {
                                self.stop();
                                self.status = "Todas as câmeras foram desconectadas.".into();
                            }
                        },
                    );
                    ui.separator();
                    let available = ui.available_size();
                    let expanded = self.expanded_slot;
                    let side = if expanded.is_some() {
                        1
                    } else {
                        self.grid_side
                    };
                    let spacing = ui.spacing().item_spacing;
                    let width =
                        ((available.x - spacing.x * (side - 1) as f32) / side as f32).max(1.0);
                    let height =
                        ((available.y - spacing.y * (side - 1) as f32) / side as f32).max(1.0);
                    ui.vertical(|ui| {
                        for row in 0..side {
                            ui.horizontal(|ui| {
                                for col in 0..side {
                                    let index = expanded.unwrap_or(row * side + col);
                                    let (rect, response) = ui.allocate_exact_size(
                                        egui::vec2(width, height),
                                        egui::Sense::click(),
                                    );
                                    if response.clicked() {
                                        self.selected_slot = index;
                                    }
                                    if response.double_clicked() {
                                        self.selected_slot = index;
                                        self.expanded_slot = toggle_expanded(expanded, index);
                                    }
                                    response.context_menu(|ui| {
                                        image_scale_menu(ui, &mut self.slots[index], rect);
                                        if self.slots[index].secondary.is_some() {
                                            ui.menu_button("Qualidade da imagem", |ui| {
                                                let current = self.slots[index].live_quality;
                                                for (quality, label) in LiveQuality::OPTIONS {
                                                    if ui
                                                        .selectable_label(current == quality, label)
                                                        .clicked()
                                                    {
                                                        if let Err(error) = self
                                                            .set_live_quality(index, quality, ctx)
                                                        {
                                                            self.status = error.into();
                                                        }
                                                        ui.close_menu();
                                                    }
                                                }
                                            });
                                        }
                                        ui.separator();
                                        if ui
                                            .add_enabled(
                                                self.slots[index].camera.is_some(),
                                                egui::Button::new("Desconectar"),
                                            )
                                            .clicked()
                                        {
                                            self.slots[index].stop();
                                            self.status =
                                                format!("Quadro {} desconectado.", index + 1);
                                            ui.close_menu();
                                        }
                                        if ui
                                            .add_enabled(
                                                self.slots.iter().any(|slot| slot.camera.is_some()),
                                                egui::Button::new("Fechar todas as câmeras"),
                                            )
                                            .clicked()
                                        {
                                            self.stop();
                                            self.status =
                                                "Todas as câmeras foram desconectadas.".into();
                                            ui.close_menu();
                                        }
                                    });
                                    ui.painter().rect_filled(
                                        rect,
                                        0.0,
                                        egui::Color32::from_rgb(20, 23, 26),
                                    );
                                    if self.slots[index].camera.is_some() {
                                        if let Some(texture) = &self.slots[index].texture {
                                            let size = self.slots[index]
                                                .framing
                                                .size(rect, texture.size());
                                            let image_rect =
                                                egui::Rect::from_center_size(rect.center(), size);
                                            ui.painter().with_clip_rect(rect).image(
                                                texture.id(),
                                                image_rect,
                                                egui::Rect::from_min_max(
                                                    egui::Pos2::ZERO,
                                                    egui::pos2(1.0, 1.0),
                                                ),
                                                egui::Color32::WHITE,
                                            );
                                        } else {
                                            ui.painter().text(
                                                rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                if self.slots[index].reconnecting {
                                                    "Conexão interrompida\nReconectando…"
                                                } else {
                                                    "Conectando…"
                                                },
                                                egui::FontId::proportional(14.0),
                                                egui::Color32::GRAY,
                                            );
                                        }
                                    } else {
                                        ui.painter().text(
                                            rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            format!("Quadro {}\nSelecione uma câmera", index + 1),
                                            egui::FontId::proportional(14.0),
                                            egui::Color32::DARK_GRAY,
                                        );
                                    }
                                    paint_slot_border(ui, rect, self.selected_slot == index);
                                }
                            });
                        }
                    });
                });
            }
        });
        if let Some(mut seconds) = self.cache_draft {
            let mut open = true;
            let mut save = false;
            let mut cancel = false;
            let mut export_config = false;
            let mut import_config = false;
            let backup_busy = self.pending_config_import.is_some();
            egui::Window::new("Configuração")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Cache da reprodução por câmera");
                        ui.add(egui::DragValue::new(&mut seconds)
                            .range(config::MIN_CACHE_SECONDS..=config::MAX_CACHE_SECONDS)
                            .suffix(" s"));
                    });
                    ui.small("Um cache maior pode reduzir travadas, mas aumenta a espera inicial.");
                    ui.small("A mudança vale ao iniciar, saltar na linha do tempo ou mudar a velocidade.");
                    if self.load_failed {
                        ui.label("O arquivo não foi carregado. Salvamento bloqueado para preservá-lo.");
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Restaurar padrão (7 s)").clicked() {
                            seconds = config::DEFAULT_CACHE_SECONDS;
                        }
                        save = ui.add_enabled(!self.load_failed, egui::Button::new("Salvar")).clicked();
                        cancel = ui.button("Cancelar").clicked();
                    });
                    ui.separator();
                    ui.small("O arquivo inclui as senhas sem criptografia. Guarde-o em local privado.");
                    ui.horizontal(|ui| {
                        export_config = ui.add_enabled(!self.load_failed && !backup_busy, egui::Button::new("Exportar configurações…")).clicked();
                        import_config = ui.add_enabled(!backup_busy, egui::Button::new("Importar configurações…")).clicked();
                    });
                });
            if save {
                match self.config_writer()(&self.equipment, seconds, &self.passwords) {
                    Ok(()) => {
                        self.playback_cache_seconds = seconds;
                        self.status = format!("Cache da reprodução salvo: {seconds} s por câmera.");
                        open = false;
                    }
                    Err(error) => self.status = error,
                }
            }
            self.cache_draft = (open && !cancel).then_some(seconds);
            if export_config {
                if let Some(file) = rfd::FileDialog::new()
                    .set_title("Exportar configurações salvas")
                    .add_filter("Arquivo JSON", &["json"])
                    .set_file_name(config::FILE_NAME)
                    .save_file()
                {
                    let snapshot = config_snapshot(
                        &self.equipment,
                        self.playback_cache_seconds,
                        &self.passwords,
                    );
                    self.status = match config::export(&file, &snapshot) {
                        Ok(()) => "Configurações exportadas.".into(),
                        Err(error) => error,
                    };
                }
            } else if import_config {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Importar configurações")
                    .add_filter("Arquivo JSON", &["json"])
                    .pick_file()
                {
                    if let Err(error) = self.prepare_import(path) {
                        self.status = error;
                    }
                }
            }
        }
        self.backup_ui(ctx);
        self.equipment_removal_ui(ctx);
        if let Some(message) = self.export_notice.clone() {
            let mut open = true;
            let mut dismiss = false;
            egui::Window::new("Resultado da exportação")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(message);
                    if ui.button("OK").clicked() {
                        dismiss = true;
                    }
                });
            if dismiss || !open {
                self.export_notice = None;
            }
        }
        if self.edit_open {
            let mut open = self.edit_open;
            egui::Window::new("Cadastro de equipamento")
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    egui::Grid::new("equipment_form")
                        .num_columns(2)
                        .show(ui, |ui| {
                            ui.label("Nome");
                            ui.text_edit_singleline(&mut self.name);
                            ui.end_row();
                            ui.label("Marca");
                            egui::ComboBox::from_id_salt("brand")
                                .selected_text(match self.brand {
                                    Brand::Intelbras => "Intelbras",
                                    Brand::Hikvision => "Hikvision",
                                    Brand::Onvif => "ONVIF",
                                    Brand::Custom => "Personalizado",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.brand,
                                        Brand::Intelbras,
                                        "Intelbras",
                                    );
                                    ui.selectable_value(
                                        &mut self.brand,
                                        Brand::Hikvision,
                                        "Hikvision",
                                    );
                                    if ui.selectable_value(&mut self.brand, Brand::Onvif, "ONVIF").clicked() {
                                        self.channel_count = 1;
                                        if self.username.is_empty() {
                                            self.username = "admin".into();
                                        }
                                    }
                                });
                            ui.end_row();
                            ui.label("IP / domínio");
                            ui.text_edit_singleline(&mut self.host);
                            ui.end_row();
                            ui.label("Porta RTSP");
                            if self.brand == Brand::Onvif {
                                ui.label("Informada pelo dispositivo");
                            } else {
                                ui.add(egui::DragValue::new(&mut self.port).range(1..=u16::MAX));
                            }
                            ui.end_row();
                            ui.label("Porta HTTP");
                            ui.add(egui::DragValue::new(&mut self.http_port).range(1..=u16::MAX));
                            ui.end_row();
                            ui.label("Fuso do equipamento (UTC)");
                            ui.add(egui::TextEdit::singleline(&mut self.equipment_utc_offset).hint_text("Automático (computador)"));
                            ui.end_row();
                            ui.label("Usuário");
                            ui.text_edit_singleline(&mut self.username);
                            ui.end_row();
                            ui.label("Senha");
                            ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                            ui.end_row();
                            ui.label("Quantidade de canais");
                            if self.brand == Brand::Onvif {
                                ui.label("1 (primeiro perfil ONVIF)");
                            } else {
                                ui.add(egui::DragValue::new(&mut self.channel_count).range(1..=256));
                            }
                            ui.end_row();
                        });
                    ui.label("A senha é salva em configs.json, sem criptografia.");
                    ui.small("Fuso: deixe vazio para usar o fuso do computador ou informe um deslocamento fixo, como -03:00. Usado na reprodução Intelbras; Hikvision e ONVIF usam UTC no protocolo. Não altera o relógio do equipamento.");
                    if self.brand == Brand::Onvif {
                        ui.label("ONVIF: a porta HTTP é usada para consultar o primeiro perfil e o endereço RTSP. Reprodução experimental.");
                    }
                    ui.label("RTSP geralmente usa a porta 554, não a porta de serviço 37777.");
                    if ui.button("Salvar").clicked() {
                        self.save();
                        if self.status.starts_with("Equipamento salvo") {
                            self.edit_open = false;
                            if let Some(index) = self.selected {
                                self.start_discovery(self.equipment[index].clone(), ctx);
                            }
                        }
                    }
                    ui.label(&self.status);
                });
            self.edit_open &= open;
        }
        if self.about_open {
            egui::Window::new("Sobre")
                .open(&mut self.about_open)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.heading("RustDVR");
                    ui.label(format!("Versão {}", env!("CARGO_PKG_VERSION")));
                    ui.label("Visualização e reprodução de câmeras e gravadores.");
                    ui.label("Licença: GNU General Public License, versão 3 (GPL-3.0).");
                    ui.hyperlink_to(
                        "Texto da licença GPL v3",
                        "https://www.gnu.org/licenses/gpl-3.0.html",
                    );
                    ui.small("Sem garantia; consulte a licença para detalhes.");
                    #[cfg(any(target_os = "linux", target_os = "windows"))]
                    if let Some(maintenance) = &mut self.maintenance {
                        ui.separator();
                        maintenance.about(ui);
                    }
                });
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        if let Some(maintenance) = &mut self.maintenance {
            maintenance.show(ctx, self.export_result.is_none());
        }
        let layout = (self.grid_side, self.expanded_slot);
        if layout != self.quality_layout {
            self.quality_layout = layout;
            self.sync_live_quality(ctx);
        }
    }
}

fn main() -> eframe::Result {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if maintenance::handle_internal_command() {
        return Ok(());
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--migrar-config")
    {
        match load_config() {
            Ok(settings) => {
                println!(
                    "Configuração verificada: {} equipamento(s), {} senha(s).",
                    settings.equipment.len(),
                    settings
                        .passwords
                        .as_ref()
                        .map_or(0, |passwords| passwords.len())
                );
                return Ok(());
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("visor-dvr")
            .with_inner_size([1000.0, 600.0]),
        ..Default::default()
    };
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let (restart_tx, restart_rx) = mpsc::channel::<maintenance::Restart>();
    eframe::run_native(
        "RustDVR",
        options,
        Box::new(move |_context| {
            let viewer = Viewer::default();
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            let viewer = {
                let mut viewer = viewer;
                viewer.maintenance = Some(maintenance::Maintenance::new(
                    &_context.egui_ctx,
                    restart_tx,
                ));
                viewer
            };
            Ok(Box::new(viewer))
        }),
    )?;
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if let Ok(restart) = restart_rx.try_recv() {
        if let Err(error) = restart.apply() {
            maintenance::report_error("Falha ao atualizar o RustDVR", &error.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_previews_the_target_while_dragging_and_only_seeks_on_release() {
        use egui::{epaint::Shape, Event, Modifiers, PointerButton, RawInput};

        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 300.0));
        let interval =
            playback::Interval::new("2024-03-01 23:59:30", "2024-03-02 00:00:30").unwrap();
        let run = |events| {
            let mut seek = None;
            let output = ctx.run(
                RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.visuals_mut().handle_shape = egui::style::HandleShape::Circle;
                        seek = playback_timeline_control(ui, interval, "UTC", 0);
                    });
                },
            );
            (output, seek)
        };
        let previews = |output: &egui::FullOutput| {
            output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    Shape::Text(text) if text.galley.text().starts_with("Ir para ") => {
                        let bounds = text.galley.rect.translate(text.pos.to_vec2());
                        assert!(screen.contains_rect(bounds));
                        Some(text.galley.text().to_owned())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let (initial, _) = run(vec![]);
        let handle = initial
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                Shape::Circle(circle) => Some(circle.center),
                _ => None,
            })
            .unwrap();
        let pointer = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let (press, seek) = run(vec![Event::PointerMoved(handle), pointer(handle, true)]);
        assert_eq!(seek, None);
        assert_eq!(previews(&press), ["Ir para 2024-03-01 23:59:30 (UTC)"]);

        let right = egui::pos2(screen.right() - 1.0, handle.y);
        let (drag, seek) = run(vec![Event::PointerMoved(right)]);
        assert_eq!(seek, None);
        assert_eq!(previews(&drag), ["Ir para 2024-03-02 00:00:29 (UTC)"]);

        let (release, seek) = run(vec![pointer(right, false)]);
        assert_eq!(seek, Some(59));
        assert!(previews(&release).is_empty());
    }

    fn playback_test_frame(elapsed: f64) -> video::Frame {
        video::Frame {
            width: 1,
            height: 1,
            rgb: vec![0; 3],
            recording_elapsed: Some(elapsed),
        }
    }

    fn playback_test_group(now: std::time::Instant) -> (PlaybackGroup, VideoSlots) {
        let interval =
            playback::Interval::new("2024-03-01 10:00:00", "2024-03-01 10:01:00").unwrap();
        let speed = playback::Speed::Normal;
        let mut cameras = [None; MAX_GRID_SLOTS];
        cameras[..2].copy_from_slice(&[Some((0, 1)), Some((0, 2))]);
        let group = PlaybackGroup {
            cameras,
            interval,
            clock: playback::SyncClock::new(0, speed, 7.0, now),
        };
        let mut slots: VideoSlots = Default::default();
        for slot in &mut slots[..2] {
            slot.playback = Some(playback::Progress::new(interval, 0, speed));
            slot.playback_buffer = Some(Default::default());
        }
        (group, slots)
    }

    fn playback_test_viewer() -> Viewer {
        Viewer::new(
            config::Config::new(Vec::new(), config::DEFAULT_CACHE_SECONDS),
            String::new(),
            false,
        )
    }

    #[test]
    fn playback_uses_local_input_and_individual_device_offsets() {
        let zone = jiff::tz::TimeZone::get("America/Sao_Paulo").unwrap();
        let local = playback::Interval::new("2026-11-19 08:22:00", "2026-11-19 08:23:00").unwrap();
        let utc = local.local_to_utc(&zone).unwrap();
        assert_eq!(
            device_playback_interval(Brand::Intelbras, None, local, utc)
                .unwrap()
                .at(0)
                .to_string(),
            "2026-11-19T08:22:00"
        );
        assert_eq!(
            device_playback_interval(Brand::Intelbras, Some(-240), local, utc)
                .unwrap()
                .at(0)
                .to_string(),
            "2026-11-19T07:22:00"
        );
        assert_eq!(
            device_playback_interval(Brand::Hikvision, Some(-240), local, utc)
                .unwrap()
                .at(0)
                .to_string(),
            "2026-11-19T11:22:00"
        );
        assert_eq!(
            device_playback_interval(Brand::Onvif, Some(-240), local, utc)
                .unwrap()
                .at(0)
                .to_string(),
            "2026-11-19T11:22:00"
        );
        assert_eq!(format_utc_offset(-210), "-03:30");
        assert!(playback::utc_offset_minutes("+14:01").is_err());
    }

    #[test]
    fn disabling_equipment_preserves_config_and_stops_only_its_cameras() {
        let mut viewer = playback_test_viewer();
        viewer.equipment = ["first", "second"]
            .map(|id| Equipment {
                id: id.into(),
                disabled: false,
                name: id.into(),
                brand: Brand::Intelbras,
                host: "example.com".into(),
                port: 554,
                http_port: 80,
                utc_offset_minutes: None,
                username: "viewer".into(),
                channels: 4,
            })
            .into();
        viewer
            .passwords
            .insert("first".into(), Zeroizing::new("secret".into()));
        viewer.playback_cameras[..2].copy_from_slice(&[Some((0, 1)), Some((1, 1))]);
        let (mut group, _) = playback_test_group(std::time::Instant::now());
        group.cameras = viewer.playback_cameras;
        viewer.playback_group = Some(group);
        viewer.slots[0].camera = Some(("first".into(), 1));
        viewer.slots[1].camera = Some(("second".into(), 1));
        viewer.playback_slots[0].camera = Some(("first".into(), 1));
        assert!(viewer
            .set_equipment_enabled(0, false, |_, _, _| Err("erro".into()))
            .is_err());
        assert!(!viewer.equipment[0].disabled);
        assert!(viewer.slots[0].camera.is_some());
        viewer
            .set_equipment_enabled(0, false, |items, _, passwords| {
                assert!(items[0].disabled);
                assert!(passwords.contains_key("first"));
                Ok(())
            })
            .unwrap();
        assert!(viewer.slots[0].camera.is_none());
        assert!(viewer.playback_slots[0].camera.is_none());
        assert!(viewer.slots[1].camera.is_some());
        assert_eq!(viewer.playback_cameras[..2], [None, Some((1, 1))]);
        assert_eq!(
            viewer.playback_group.unwrap().cameras[..2],
            [None, Some((1, 1))]
        );
        viewer.selected = Some(0);
        assert!(viewer.connect(&egui::Context::default()).is_err());
        viewer
            .set_equipment_enabled(0, true, |_, _, _| Ok(()))
            .unwrap();
        assert!(!viewer.equipment[0].disabled);
    }

    #[test]
    fn disabling_waits_for_discovery_and_export() {
        let mut viewer = playback_test_viewer();
        let equipment: Equipment = serde_json::from_str(r#"{"id":"a","name":"a","brand":"Intelbras","host":"example.com","port":554,"username":"u","channels":1}"#).unwrap();
        assert!(!equipment.disabled);
        viewer.equipment.push(equipment);
        viewer
            .discovery
            .insert("a".into(), (1, discovery::State::Checking));
        assert!(viewer
            .set_equipment_enabled(0, false, |_, _, _| panic!("não deve salvar"))
            .is_err());
        viewer.discovery.clear();
        let (_tx, rx) = mpsc::channel();
        viewer.export_result = Some(rx);
        assert!(viewer
            .set_equipment_enabled(0, false, |_, _, _| panic!("não deve salvar"))
            .is_err());
        viewer.export_result = None;
        viewer
            .set_equipment_enabled(0, false, |items, _, _| {
                let json = serde_json::to_string(&items[0]).unwrap();
                assert!(serde_json::from_str::<Equipment>(&json).unwrap().disabled);
                Ok(())
            })
            .unwrap();
        viewer.start_discovery(viewer.equipment[0].clone(), &egui::Context::default());
        assert!(viewer.discovery.is_empty());
    }

    #[test]
    fn removing_equipment_saves_first_and_preserves_other_selections() {
        let mut viewer = playback_test_viewer();
        viewer.equipment = ["first", "second", "third"]
            .map(|id| Equipment {
                id: id.into(),
                disabled: false,
                name: id.into(),
                brand: Brand::Intelbras,
                host: "example.com".into(),
                port: 554,
                http_port: 80,
                utc_offset_minutes: None,
                username: "viewer".into(),
                channels: 4,
            })
            .into();
        for id in ["second", "second:video", "third"] {
            viewer
                .passwords
                .insert(id.into(), Zeroizing::new("secret".into()));
        }
        viewer.selected = Some(2);
        viewer.playback_cameras[..3].copy_from_slice(&[Some((0, 1)), Some((1, 2)), Some((2, 3))]);
        let (mut group, _) = playback_test_group(std::time::Instant::now());
        group.cameras = viewer.playback_cameras;
        viewer.playback_group = Some(group);
        viewer.slots[0].camera = Some(("second".into(), 1));
        viewer.slots[1].camera = Some(("third".into(), 1));
        viewer.playback_slots[1].camera = Some(("second".into(), 2));
        assert!(viewer
            .remove_equipment("second", |_, _, _| Err("erro".into()))
            .is_err());
        assert_eq!(viewer.equipment.len(), 3);
        assert!(viewer.slots[0].camera.is_some());
        assert!(viewer.passwords.contains_key("second"));
        viewer
            .remove_equipment("second", |equipment, _, passwords| {
                assert_eq!(
                    equipment.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
                    ["first", "third"]
                );
                assert!(!passwords.contains_key("second"));
                assert!(!passwords.contains_key("second:video"));
                assert!(passwords.contains_key("third"));
                Ok(())
            })
            .unwrap();
        let expected = [Some((0, 1)), None, Some((1, 3))];
        assert_eq!(viewer.playback_cameras[..3], expected);
        assert_eq!(viewer.playback_group.unwrap().cameras[..3], expected);
        assert_eq!(viewer.selected, Some(1));
        assert!(viewer.slots[0].camera.is_none());
        assert!(viewer.playback_slots[1].camera.is_none());
        assert_eq!(viewer.slots[1].camera.as_ref().unwrap().0, "third");
        viewer.load_failed = true;
        assert!(viewer
            .remove_equipment("third", |_, _, _| panic!("não deve salvar"))
            .is_err());
    }

    #[test]
    fn reordering_equipment_preserves_current_selections_and_streams() {
        let mut viewer = playback_test_viewer();
        viewer.equipment = ["first", "second", "third"]
            .map(|id| Equipment {
                id: id.into(),
                disabled: false,
                name: id.into(),
                brand: Brand::Intelbras,
                host: "example.com".into(),
                port: 554,
                http_port: 80,
                utc_offset_minutes: None,
                username: "viewer".into(),
                channels: 4,
            })
            .into();
        viewer.selected = Some(1);
        viewer.playback_cameras[..3].copy_from_slice(&[Some((0, 1)), Some((1, 2)), Some((2, 3))]);
        let (mut group, slots) = playback_test_group(std::time::Instant::now());
        group.cameras = viewer.playback_cameras;
        viewer.playback_group = Some(group);
        viewer.playback_slots = slots;
        let (cancel, mut worker) = tokio::sync::oneshot::channel();
        viewer.playback_slots[0].cancel = Some(cancel);
        viewer.slots[0].camera = Some(("first".into(), 1));
        viewer.playback_cache_seconds = 12;
        viewer
            .swap_equipment(0, 1, |equipment, cache, _| {
                assert_eq!(
                    equipment.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
                    ["second", "first", "third"]
                );
                assert_eq!(cache, 12);
                Ok(())
            })
            .unwrap();
        let expected = [Some((1, 1)), Some((0, 2)), Some((2, 3))];
        assert_eq!(viewer.selected, Some(0));
        assert_eq!(viewer.playback_cameras[..3], expected);
        assert_eq!(viewer.playback_group.unwrap().cameras[..3], expected);
        assert_eq!(viewer.slots[0].camera, Some(("first".into(), 1)));
        assert!(matches!(
            worker.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        let failed = viewer.swap_equipment(0, 1, |_, _, _| Err("Falha na gravação.".into()));
        assert!(failed.is_err());
        assert_eq!(viewer.equipment[0].id, "second");
        assert_eq!(viewer.selected, Some(0));
        assert_eq!(viewer.playback_group.unwrap().cameras[..3], expected);
    }

    #[test]
    fn opening_playback_populates_local_dates_once_and_keeps_manual_edits() {
        let mut viewer = playback_test_viewer();
        viewer.open_playback_tab(|| "2024-03-01T12:15:43-03:00[-03:00]".parse().unwrap());
        assert_eq!(viewer.playback_start, "2024-03-01 07:30:00");
        assert_eq!(viewer.playback_end, "2024-03-01 12:15:43");

        viewer.playback_start = "2024-03-01 11:00:00".into();
        assert_eq!(viewer.playback_start, "2024-03-01 11:00:00");
        assert_eq!(viewer.playback_end, "2024-03-01 12:15:43");

        viewer.open_playback_tab(|| panic!("a aba já está aberta"));
        assert_eq!(viewer.playback_end, "2024-03-01 12:15:43");
        viewer.playback_tab_open = false;
        viewer.open_playback_tab(|| "2024-03-02T12:17:00-03:00[-03:00]".parse().unwrap());
        assert_eq!(viewer.playback_start, "2024-03-02 07:30:00");
        assert_eq!(viewer.playback_end, "2024-03-02 12:17:00");
    }

    #[test]
    fn playback_renders_grade_options_at_the_bottom_of_the_left_sidebar() {
        use egui::epaint::Shape;

        let ctx = egui::Context::default();
        let mut viewer = playback_test_viewer();
        viewer.set_playback_grid_side(5);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 600.0));
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| viewer.playback_ui(ui, ctx));
            },
        );
        let text = |expected| {
            output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    Shape::Text(text) if text.galley.text() == expected => {
                        Some((text.pos, shape.clip_rect))
                    }
                    _ => None,
                })
                .unwrap()
        };
        for label in ["4", "9", "16", "25"] {
            let (pos, clip) = text(label);
            assert!(pos.x < 220.0);
            assert!(pos.y > screen.bottom() - 60.0);
            assert!(clip.contains(pos));
        }
        assert!(text("Quadro 25").0.x > 220.0);
    }

    #[test]
    fn assigning_cameras_advances_and_wraps_in_all_playback_grids() {
        let mut viewer = playback_test_viewer();
        assert_eq!(viewer.playback_grid_side, 2);
        for side in GRID_SIDES {
            viewer.set_playback_grid_side(side);
            for index in 0..side * side {
                viewer.assign_playback_camera((0, index as u32 + 1));
                assert_eq!(viewer.playback_cameras[index], Some((0, index as u32 + 1)));
                assert_eq!(viewer.playback_slot, (index + 1) % (side * side));
            }
        }
        assert_eq!(viewer.visible_playback_cameras()[24], Some((0, 25)));
    }

    #[test]
    fn expanding_and_restoring_a_playback_slot_keeps_all_sessions() {
        let mut viewer = playback_test_viewer();
        viewer.set_playback_grid_side(3);
        let (mut group, mut slots) = playback_test_group(std::time::Instant::now());
        group.cameras[8] = Some((0, 9));
        let (visible_cancel, mut visible_worker) = tokio::sync::oneshot::channel();
        let (other_cancel, mut other_worker) = tokio::sync::oneshot::channel();
        slots[8].cancel = Some(visible_cancel);
        slots[0].cancel = Some(other_cancel);
        viewer.playback_slots = slots;
        viewer.playback_group = Some(group);

        viewer.toggle_playback_expansion(8);
        assert_eq!(viewer.playback_expanded_slot, Some(8));
        viewer.assign_playback_camera((0, 10));
        assert_eq!(viewer.playback_cameras[8], Some((0, 10)));
        assert_eq!(viewer.playback_slot, 8);
        assert_eq!(viewer.playback_group.unwrap().cameras[8], Some((0, 9)));
        viewer.toggle_playback_expansion(8);
        assert_eq!(viewer.playback_expanded_slot, None);
        assert_eq!(viewer.playback_grid_side, 3);
        assert!(matches!(
            visible_worker.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            other_worker.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn shrinking_playback_grid_stops_only_hidden_sessions_and_keeps_assignments() {
        let mut viewer = playback_test_viewer();
        viewer.set_playback_grid_side(3);
        let (mut group, mut slots) = playback_test_group(std::time::Instant::now());
        group.cameras[8] = Some((0, 9));
        viewer.playback_cameras = group.cameras;
        let (kept_cancel, mut kept_worker) = tokio::sync::oneshot::channel();
        let (hidden_cancel, mut hidden_worker) = tokio::sync::oneshot::channel();
        slots[0].cancel = Some(kept_cancel);
        slots[8].cancel = Some(hidden_cancel);
        viewer.playback_slots = slots;
        viewer.playback_group = Some(group);
        viewer.toggle_playback_expansion(8);

        viewer.set_playback_grid_side(2);
        assert_eq!(viewer.playback_expanded_slot, None);
        assert_eq!(viewer.playback_slot, 0);
        assert_eq!(viewer.playback_cameras[8], Some((0, 9)));
        assert_eq!(viewer.visible_playback_cameras()[8], None);
        assert_eq!(viewer.playback_group.unwrap().cameras[0], Some((0, 1)));
        assert_eq!(viewer.playback_group.unwrap().cameras[8], None);
        assert_eq!(hidden_worker.try_recv(), Ok(()));
        assert!(matches!(
            kept_worker.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        viewer.set_playback_grid_side(3);
        assert_eq!(viewer.visible_playback_cameras()[8], Some((0, 9)));
        assert_eq!(viewer.playback_group.unwrap().cameras[8], None);
    }

    #[tokio::test]
    async fn shared_clock_waits_for_and_displays_the_twenty_fifth_playback_slot() {
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let (mut group, mut slots) = playback_test_group(now);
        group.cameras[24] = Some((0, 25));
        slots[24].playback = Some(playback::Progress::new(
            group.interval,
            0,
            group.clock.speed(),
        ));
        let buffer = std::sync::Arc::new(video::PlaybackBuffer::default());
        slots[24].playback_buffer = Some(buffer.clone());
        let (cancel, _worker) = tokio::sync::oneshot::channel();
        slots[24].cancel = Some(cancel);

        let ctx = egui::Context::default();
        update_group(&mut group, &mut slots, &ctx, now);
        assert!(!group.clock.started());
        buffer.push(playback_test_frame(0.0)).await;
        buffer.mark_source_ready();
        update_group(&mut group, &mut slots, &ctx, now + Duration::from_secs(1));
        assert!(group.clock.started());
        assert_eq!(slots[24].displayed_position, Some(0.0));
        assert!(slots[24].texture.is_some());
    }

    #[tokio::test]
    async fn waits_for_prefill_but_starts_when_a_camera_is_slow() {
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let (mut group, mut slots) = playback_test_group(now);
        let _workers = [0, 1].map(|index| {
            let (cancel, worker) = tokio::sync::oneshot::channel();
            slots[index].cancel = Some(cancel);
            worker
        });
        let buffer = slots[0].playback_buffer.as_ref().unwrap().clone();
        let ctx = egui::Context::default();
        buffer.push(playback_test_frame(0.0)).await;
        update_group(&mut group, &mut slots, &ctx, now);
        assert!(!group.clock.started());
        assert!(slots[0].texture.is_none());

        for time in [0.25, 0.5, 0.75, 1.0] {
            buffer.push(playback_test_frame(time)).await;
        }
        buffer.mark_source_ready();
        update_group(&mut group, &mut slots, &ctx, now + Duration::from_secs(1));
        assert!(!group.clock.started());
        let start = now + playback::startup_grace(7.0);
        update_group(&mut group, &mut slots, &ctx, start);
        assert_eq!(slots[0].displayed_position, Some(0.0));
        for millis in [250, 500, 750, 1000] {
            update_group(
                &mut group,
                &mut slots,
                &ctx,
                start + Duration::from_millis(millis),
            );
            assert_eq!(slots[0].displayed_position, Some(millis as f64 / 1000.0));
            assert!(slots[1].texture.is_none());
        }
    }

    #[tokio::test]
    async fn group_starts_as_soon_as_both_queues_are_ready() {
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let (mut group, mut slots) = playback_test_group(now);
        let _workers = [0, 1].map(|index| {
            let (cancel, worker) = tokio::sync::oneshot::channel();
            slots[index].cancel = Some(cancel);
            worker
        });
        for slot in &slots[..2] {
            let buffer = slot.playback_buffer.as_ref().unwrap();
            for time in [0.0, 0.25, 0.5] {
                buffer.push(playback_test_frame(time)).await;
            }
            buffer.mark_source_ready();
        }
        let ctx = egui::Context::default();
        update_group(&mut group, &mut slots, &ctx, now + Duration::from_secs(1));
        assert!(group.clock.started());
        assert!(slots[..2]
            .iter()
            .all(|slot| slot.displayed_position == Some(0.0)));
        update_group(&mut group, &mut slots, &ctx, now + Duration::from_secs(61));
        assert!(slots[..2]
            .iter()
            .all(|slot| slot.playback_buffer.is_none() && slot.texture.is_some()));
    }

    #[tokio::test]
    async fn group_holds_future_frames_and_keeps_last_image_during_a_gap() {
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let (mut group, mut slots) = playback_test_group(now);
        let speed = group.clock.speed();
        let first = slots[0].playback_buffer.as_ref().unwrap().clone();
        let second = slots[1].playback_buffer.as_ref().unwrap().clone();
        first.push(playback_test_frame(0.0)).await;
        second.push(playback_test_frame(0.0)).await;
        let ctx = egui::Context::default();
        update_group(&mut group, &mut slots, &ctx, now);
        assert_eq!(slots[1].displayed_position, Some(0.0));
        let last_texture = slots[1].texture.as_ref().unwrap().id();

        first.push(playback_test_frame(1.0)).await;
        second.push(playback_test_frame(5.0)).await;
        update_group(&mut group, &mut slots, &ctx, now + Duration::from_secs(1));
        assert_eq!(slots[0].displayed_position, Some(1.0));
        assert_eq!(slots[1].displayed_position, Some(0.0));
        assert_eq!(slots[1].texture.as_ref().unwrap().id(), last_texture);
        assert!(slots[1].has_recent_playback_frame(now + Duration::from_secs(1), 1.0, speed));
        assert!(!slots[1].has_recent_playback_frame(now + Duration::from_secs(4), 4.0, speed));

        update_group(&mut group, &mut slots, &ctx, now + Duration::from_secs(5));
        assert_eq!(slots[1].displayed_position, Some(5.0));

        second.push(playback_test_frame(6.0)).await;
        update_group(&mut group, &mut slots, &ctx, now + Duration::from_secs(14));
        assert_eq!(slots[1].displayed_position, Some(6.0));
        assert!(slots[1].has_recent_playback_frame(now + Duration::from_secs(14), 14.0, speed));

        second.push(playback_test_frame(7.0)).await;
        update_group(&mut group, &mut slots, &ctx, now + Duration::from_secs(18));
        assert_eq!(slots[1].displayed_position, Some(6.0));
        assert_eq!(slots[1].texture.as_ref().unwrap().id(), last_texture);
        assert!(!slots[1].has_recent_playback_frame(now + Duration::from_secs(18), 18.0, speed));
    }

    #[test]
    fn playback_frames_update_position_and_interruption_keeps_it_seekable() {
        let interval =
            playback::Interval::new("2024-03-01 10:00:00", "2024-03-01 10:01:00").unwrap();
        let (frames_tx, frames) = mpsc::sync_channel(1);
        let (errors_tx, errors) = mpsc::channel();
        let mut slots = [VideoSlot {
            frames: Some(frames),
            errors: Some(errors),
            camera: Some(("dvr".into(), 1)),
            playback: Some(playback::Progress::new(
                interval,
                10,
                playback::Speed::Normal,
            )),
            ..Default::default()
        }];
        frames_tx
            .send(video::Event::Frame(video::Frame {
                width: 1,
                height: 1,
                rgb: vec![0; 3],
                recording_elapsed: Some(4.5),
            }))
            .unwrap();
        errors_tx
            .send(VideoMessage::Failed("A transmissão foi encerrada."))
            .unwrap_or_else(|_| panic!("O quadro deve receber o resultado do trabalhador"));
        let ctx = egui::Context::default();
        let mut status = String::new();
        update_slots(&mut slots, &ctx, &mut status, "Reprodução");
        assert_eq!(slots[0].playback.unwrap().offset(), 14);
        assert!(slots[0].frames.is_none());
        assert!(status.contains("A transmissão foi encerrada."));
        slots[0].stop();
        assert!(slots[0].playback.is_none());
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(
            (0..7).map(retry_delay).collect::<Vec<_>>(),
            [2, 4, 8, 16, 32, 60, 60]
        );
    }

    #[test]
    fn image_scale_fits_and_adjusts_dimensions_independently() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        assert_eq!(
            ImageFraming::Original.size(rect, [1600, 900]),
            egui::vec2(800.0, 450.0)
        );
        assert_eq!(
            ImageFraming::Custom(ImageScale {
                width: 0.5,
                height: 2.0,
            })
            .size(rect, [1600, 900]),
            egui::vec2(400.0, 900.0)
        );
        assert_eq!(
            ImageFraming::Original.size(rect, [900, 1600]),
            egui::vec2(337.5, 600.0)
        );
    }

    #[test]
    fn default_framing_fills_wide_and_tall_slots() {
        for size in [egui::vec2(800.0, 200.0), egui::vec2(200.0, 800.0)] {
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            for pixels in [[1600, 900], [900, 1600]] {
                assert_eq!(VideoSlot::default().framing.size(rect, pixels), size);
            }
        }
    }

    #[test]
    fn window_resize_restores_fill_in_both_tabs_but_repaint_does_not() {
        let mut live = [VideoSlot {
            framing: ImageFraming::Original,
            ..Default::default()
        }];
        let mut playback = [VideoSlot {
            framing: ImageFraming::Custom(ImageScale::default()),
            ..Default::default()
        }];
        let size = egui::vec2(800.0, 600.0);
        let mut previous = Some(size);
        reset_framing_after_resize(&mut previous, size, &mut live, &mut playback);
        assert!(matches!(live[0].framing, ImageFraming::Original));
        assert!(matches!(playback[0].framing, ImageFraming::Custom(_)));

        reset_framing_after_resize(&mut previous, size * 2.0, &mut live, &mut playback);
        assert!(matches!(live[0].framing, ImageFraming::Fill));
        assert!(matches!(playback[0].framing, ImageFraming::Fill));
    }

    #[test]
    fn clearing_slot_resets_framing_for_the_next_camera() {
        let mut slot = VideoSlot {
            framing: ImageFraming::Original,
            ..Default::default()
        };
        slot.stop();
        assert!(matches!(slot.framing, ImageFraming::Fill));
    }

    #[test]
    fn export_completion_is_processed_even_when_progress_is_queued() {
        let (tx, rx) = mpsc::channel();
        let mut receiver = Some(rx);
        let mut status = String::new();
        tx.send((false, "Em andamento")).unwrap();
        tx.send((true, "Exportação finalizada")).unwrap();
        assert_eq!(
            poll_export(&mut receiver, &mut status).as_deref(),
            Some("Exportação finalizada")
        );
        assert!(receiver.is_none());
        assert_eq!(status, "Exportação finalizada");
    }

    #[test]
    fn export_interruption_releases_button_and_reports_problem() {
        let (tx, rx) = mpsc::channel();
        let mut receiver = Some(rx);
        let mut status = String::new();
        tx.send((false, "Em andamento")).unwrap();
        drop(tx);
        let notice = poll_export(&mut receiver, &mut status).unwrap();
        assert!(notice.contains("interrompida"));
        assert!(receiver.is_none());
    }

    #[test]
    fn active_export_keeps_button_disabled() {
        let (tx, rx) = mpsc::channel();
        let mut receiver = Some(rx);
        let mut status = String::new();
        tx.send((false, "Em andamento")).unwrap();
        assert!(poll_export(&mut receiver, &mut status).is_none());
        assert!(receiver.is_some());
        assert_eq!(status, "Em andamento");
    }

    #[test]
    fn live_stream_quality_follows_grid_and_expansion() {
        let automatic = VideoSlot::default().live_quality;
        for index in 0..4 {
            assert!(!automatic.use_secondary(2, None, index));
        }
        for side in 3..=MAX_GRID_SIDE {
            for index in 0..side * side {
                assert!(automatic.use_secondary(side, None, index));
                assert_eq!(automatic.use_secondary(side, Some(5), index), index != 5);
            }
        }
        assert!(!automatic.use_secondary(2, Some(1), 1));
    }

    #[test]
    fn manual_live_quality_overrides_grid_and_expansion() {
        for side in GRID_SIDES {
            for expanded in [None, Some(0), Some(1)] {
                assert!(!LiveQuality::High.use_secondary(side, expanded, 0));
                assert!(LiveQuality::Low.use_secondary(side, expanded, 0));
            }
        }
    }

    #[test]
    fn clearing_live_slot_restores_automatic_quality_for_the_next_camera() {
        let mut slot = VideoSlot {
            live_quality: LiveQuality::Low,
            secondary: Some(true),
            ..Default::default()
        };
        slot.stop();
        assert!(matches!(slot.live_quality, LiveQuality::Auto));
        assert_eq!(slot.secondary, None);
    }

    #[test]
    fn double_click_toggles_expansion_without_affecting_other_slots() {
        let mut slots: VideoSlots = Default::default();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        slots[1].camera = Some(("dvr".into(), 2));
        slots[1].cancel = Some(tx);
        let expanded = toggle_expanded(None, 3);
        assert_eq!(expanded, Some(3));
        assert_eq!(toggle_expanded(expanded, 3), None);
        assert_eq!(slots[1].camera, Some(("dvr".into(), 2)));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn selection_advances_and_wraps_for_every_grid_size() {
        for side in 1..=MAX_GRID_SIDE {
            let count = side * side;
            assert_eq!(next_slot(count - 1, side), 0);
            if count > 1 {
                assert_eq!(next_slot(0, side), 1);
            }
        }
    }

    #[test]
    fn reducing_grid_disconnects_only_hidden_slots() {
        let mut slots: VideoSlots = Default::default();
        let (visible_tx, mut visible_rx) = tokio::sync::oneshot::channel();
        let (hidden_tx, mut hidden_rx) = tokio::sync::oneshot::channel();
        slots[3].camera = Some(("dvr".into(), 4));
        slots[3].cancel = Some(visible_tx);
        slots[24].camera = Some(("dvr".into(), 25));
        slots[24].cancel = Some(hidden_tx);
        let mut selected = 24;
        resize_grid(&mut slots, &mut selected, 2);
        assert_eq!(selected, 0);
        assert_eq!(hidden_rx.try_recv(), Ok(()));
        assert!(matches!(
            visible_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(slots[3].camera.is_some());
        assert!(slots[24].camera.is_none());
    }

    #[test]
    fn stopping_one_slot_does_not_stop_another() {
        let mut slots: [VideoSlot; 4] = std::array::from_fn(|_| VideoSlot::default());
        let (first_tx, mut first_rx) = tokio::sync::oneshot::channel();
        let (second_tx, mut second_rx) = tokio::sync::oneshot::channel();
        slots[0].camera = Some(("dvr".into(), 1));
        slots[0].cancel = Some(first_tx);
        slots[1].camera = Some(("dvr".into(), 2));
        slots[1].cancel = Some(second_tx);

        slots[0].stop();
        assert_eq!(first_rx.try_recv(), Ok(()));
        assert!(matches!(
            second_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(slots[0].camera.is_none());
        assert_eq!(slots[1].camera, Some(("dvr".into(), 2)));
    }

    #[test]
    fn intelbras_url_encodes_credentials() {
        let url = camera_url(
            Brand::Intelbras,
            "192.168.1.10",
            554,
            "admin",
            "a@b:c",
            2,
            true,
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "rtsp://admin:a%40b%3Ac@192.168.1.10:554/cam/realmonitor?channel=2&subtype=1"
        );
    }

    #[test]
    fn hikvision_url_uses_channel_and_stream() {
        let url = camera_url(
            Brand::Hikvision,
            "dvr.local",
            554,
            "admin",
            "senha",
            12,
            false,
        )
        .unwrap();
        assert_eq!(url.path(), "/Streaming/Channels/1201");
    }

    #[test]
    fn rejects_invalid_host_and_channel() {
        assert!(camera_url(Brand::Intelbras, "host/path", 554, "", "", 1, false).is_err());
        assert!(camera_url(Brand::Intelbras, "host", 554, "", "", 0, false).is_err());
    }

    #[test]
    fn onvif_click_builds_http_device_url_without_credentials() {
        let rtsp = camera_url(Brand::Onvif, "192.0.2.1", 554, "admin", "secret", 1, false).unwrap();
        let device = onvif_device_url(&rtsp, 8080).unwrap();
        assert_eq!(
            device.as_str(),
            "http://192.0.2.1:8080/onvif/device_service"
        );
        assert!(device.username().is_empty());
        assert!(device.password().is_none());
        let ipv6 = camera_url(Brand::Onvif, "::1", 554, "", "", 1, false).unwrap();
        assert_eq!(
            onvif_device_url(&ipv6, 80).unwrap().as_str(),
            "http://[::1]/onvif/device_service"
        );
    }

    #[test]
    fn imports_legacy_ezviz_as_onvif_without_losing_credentials() {
        let settings: LocalConfig = config::parse(br#"{
            "equipment":[{"id":"camera","name":"EZ","brand":"Ezviz","host":"camera.local","port":554,"username":"admin","channels":1}],
            "passwords":{"camera":"access","camera:video":"video","other:video":"orphan"}
        }"#).unwrap();
        let validated = validate_import(settings).unwrap();
        let passwords = validated.passwords.unwrap();
        assert_eq!(
            passwords.get("camera:video").map(|p| p.as_str()),
            Some("video")
        );
        assert!(!passwords.contains_key("other:video"));
        assert!(validated.equipment[0].brand == Brand::Onvif);
        assert_eq!(
            serde_json::to_string(&validated.equipment[0].brand).unwrap(),
            "\"Onvif\""
        );
    }

    #[test]
    fn exported_configuration_restores_passwords_order_and_cache_on_another_installation() {
        let dir = std::env::temp_dir().join(format!("visor-dvr-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join(config::FILE_NAME);
        let old = br#"{"playback_cache_seconds":7,"equipment":[],"passwords":{}}"#;
        std::fs::write(&path, old).unwrap();
        let settings: LocalConfig = config::parse(br#"{
            "playback_cache_seconds": 15,
            "equipment": [
                {"id":"b","name":"B","brand":"Intelbras","host":"b.example","port":554,"username":"viewer","channels":4},
                {"id":"a","name":"A","brand":"Hikvision","host":"a.example","port":554,"username":"viewer","channels":4}
            ],
            "passwords":{"a":"synthetic-a","b":"synthetic-b"}
        }"#).unwrap();
        let exported = dir.join("backup.json");
        let snapshot = config_snapshot(
            &settings.equipment,
            settings.playback_cache_seconds,
            &settings.passwords.unwrap(),
        );
        config::export(&exported, &snapshot).unwrap();
        let mut viewer = playback_test_viewer();
        viewer.prepare_import(exported.clone()).unwrap();
        assert!(viewer.equipment.is_empty());
        let pending = viewer.pending_config_import.take().unwrap();
        viewer.apply_config_import(&path, pending).unwrap();
        assert_eq!(viewer.playback_cache_seconds, 15);
        assert_eq!(
            viewer
                .equipment
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(
            viewer.equipment_password("b").as_deref(),
            Some("synthetic-b")
        );
        assert!(viewer.discovery_started);
        let reloaded = config::read_import::<Equipment>(&path).unwrap();
        assert_eq!(reloaded.passwords, snapshot.passwords);
        let copied = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".bak.json"))
            .unwrap();
        assert_eq!(std::fs::read(copied.path()).unwrap(), old);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in [&path, &exported, &copied.path()] {
                assert_eq!(
                    std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saving_cache_preserves_old_equipment_and_its_keyring_id() {
        let dir = std::env::temp_dir().join(format!("visor-dvr-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("equipamentos.json");
        let legacy = br#"[{"id":"existing-id","name":"Teste","brand":"Intelbras","host":"example.com","port":554,"username":"viewer","channels":4,"password":"not-serialized"}]"#;
        std::fs::write(&path, legacy).unwrap();
        let loaded = config::read_import::<Equipment>(&path).unwrap();
        assert_eq!(loaded.playback_cache_seconds, 7);
        assert_eq!(std::fs::read(&path).unwrap(), legacy);

        config::save(&path, &config::Config::new(&loaded.equipment, 18)).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("password"));
        let reloaded = config::read_import::<Equipment>(&path).unwrap();
        assert_eq!(reloaded.playback_cache_seconds, 18);
        assert_eq!(reloaded.equipment.len(), 1);
        assert_eq!(reloaded.equipment[0].id, "existing-id");
        assert_eq!(reloaded.equipment[0].channels, 4);
        assert_eq!(reloaded.equipment[0].http_port, 80);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn old_equipment_loads_without_id_or_password_in_json() {
        let old = r#"{"name":"DVR","brand":"Intelbras","host":"dvr.local","port":554,"username":"admin","channels":4}"#;
        let equipment: Equipment = serde_json::from_str(old).unwrap();
        assert!(equipment.id.is_empty());
        let json = serde_json::to_string(&equipment).unwrap();
        assert!(!json.contains("password"));
    }
}
