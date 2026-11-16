use eframe::egui;
mod discovery;
mod playback;
mod video;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::{
    sync::mpsc::{self, Receiver},
    thread,
};
use url::Url;

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
    Custom,
}

#[derive(Clone, Serialize, Deserialize)]
struct Equipment {
    #[serde(default)]
    id: String,
    name: String,
    brand: Brand,
    host: String,
    port: u16,
    #[serde(default = "default_http_port")]
    http_port: u16,
    username: String,
    channels: u32,
}

fn default_http_port() -> u16 {
    80
}

fn equipment_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("br", "visor-dvr", "visor-dvr")
        .map(|dirs| dirs.config_dir().join("equipamentos.json"))
}

fn load_equipment() -> Result<Vec<Equipment>, String> {
    let path = equipment_path().ok_or("Pasta de configuração indisponível.")?;
    match std::fs::read(path) {
        Ok(data) => {
            let mut equipment: Vec<Equipment> = serde_json::from_slice(&data)
                .map_err(|_| "Não foi possível ler o cadastro de equipamentos.")?;
            let mut changed = false;
            for item in &mut equipment {
                if item.id.is_empty() {
                    item.id = uuid::Uuid::new_v4().to_string();
                    changed = true;
                }
            }
            if changed {
                save_equipment(&equipment)
                    .map_err(|_| "Não foi possível atualizar o cadastro existente.")?;
            }
            Ok(equipment)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(_) => Err("Não foi possível abrir o cadastro de equipamentos.".into()),
    }
}

fn credential(id: &str) -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new("br.visor-dvr.equipamentos", id)
}

fn save_equipment(equipment: &[Equipment]) -> Result<(), String> {
    let path = equipment_path().ok_or("Pasta de configuração indisponível.")?;
    let parent = path.parent().ok_or("Pasta de configuração inválida.")?;
    std::fs::create_dir_all(parent)
        .map_err(|_| "Não foi possível criar a pasta de configuração.")?;
    let data = serde_json::to_vec_pretty(equipment).map_err(|_| "Erro ao preparar cadastro.")?;
    // Preserve the previous file if writing fails.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, data).map_err(|_| "Não foi possível salvar o cadastro.")?;
    std::fs::rename(&temporary, &path).map_err(|_| "Não foi possível atualizar o cadastro.")?;
    Ok(())
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
        Brand::Custom => return Err("Informe uma URL RTSP personalizada."),
    }
    Ok(url)
}

#[derive(Default)]
struct VideoSlot {
    camera: Option<(String, u32)>,
    secondary: Option<bool>,
    frames: Option<Receiver<video::Event>>,
    errors: Option<Receiver<&'static str>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    texture: Option<egui::TextureHandle>,
}

impl VideoSlot {
    fn stop(&mut self) {
        self.frames = None;
        self.errors = None;
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        self.texture = None;
        self.camera = None;
        self.secondary = None;
    }

    fn start(
        &mut self,
        url: Url,
        credentials: retina::client::Credentials,
        camera: (String, u32),
        ctx: &egui::Context,
    ) {
        self.stop();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (error_sender, error_receiver) = mpsc::channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = error_sender.send("Não foi possível iniciar o vídeo.");
                ctx.request_repaint();
                return;
            };
            runtime.block_on(async {
                tokio::select! {
                    _ = cancel_rx => {},
                    result = video::run(url, credentials, sender, Some(ctx.clone())) => {
                        if let Err(error) = result { let _ = error_sender.send(error); ctx.request_repaint(); }
                    }
                }
            });
        });
        self.frames = Some(receiver);
        self.errors = Some(error_receiver);
        self.cancel = Some(cancel_tx);
        self.camera = Some(camera);
    }
}

fn resize_grid(slots: &mut [VideoSlot; 16], selected: &mut usize, side: usize) {
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

fn use_secondary(side: usize, expanded: Option<usize>, index: usize) -> bool {
    side >= 3 && expanded != Some(index)
}

struct Viewer {
    tab: Tab,
    equipment_tab_open: bool,
    live_tab_open: bool,
    playback_tab_open: bool,
    edit_open: bool,
    selected_slot: usize,
    slots: [VideoSlot; 16],
    grid_side: usize,
    expanded_slot: Option<usize>,
    quality_layout: (usize, Option<usize>),
    playback_slots: [VideoSlot; 4],
    playback_slot: usize,
    playback_camera: Option<(usize, u32)>,
    playback_start: String,
    playback_end: String,
    equipment: Vec<Equipment>,
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
}

impl Default for Viewer {
    fn default() -> Self {
        let (equipment, status, load_failed) = match load_equipment() {
            Ok(equipment) => (
                equipment,
                "Cadastre ou selecione um equipamento.".into(),
                false,
            ),
            Err(error) => (Vec::new(), error, true),
        };
        let (discovery_tx, discovery_rx) = mpsc::channel();
        Self {
            tab: Tab::Live,
            equipment_tab_open: false,
            live_tab_open: true,
            playback_tab_open: false,
            edit_open: false,
            selected_slot: 0,
            slots: std::array::from_fn(|_| VideoSlot::default()),
            grid_side: 2,
            expanded_slot: None,
            quality_layout: (2, None),
            playback_slots: std::array::from_fn(|_| VideoSlot::default()),
            playback_slot: 0,
            playback_camera: None,
            playback_start: String::new(),
            playback_end: String::new(),
            equipment,
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
        }
    }
}

impl Viewer {
    fn start_discovery(&mut self, equipment: Equipment, ctx: &egui::Context) {
        self.next_discovery += 1;
        let generation = self.next_discovery;
        self.discovery.insert(
            equipment.id.clone(),
            (generation, discovery::State::Checking),
        );
        let sender = self.discovery_tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let state = match credential(&equipment.id).and_then(|entry| entry.get_password()) {
                Ok(password) => discovery::discover(&equipment, password),
                Err(_) => discovery::State::Unknown("Senha indisponível no armazenamento seguro."),
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
                    ui.heading("Reprodução");
                    ui.label("Selecione uma câmera");
                    let list_height = (ui.available_height() - 230.0).max(50.0);
                    let mut playback_selection = None;
                    egui::ScrollArea::vertical()
                        .id_salt("playback_tree")
                        .max_height(list_height)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (index, item) in self.equipment.iter().enumerate() {
                                egui::CollapsingHeader::new(self.device_title(item))
                                    .id_salt(("playback_equipment", index))
                                    .show(ui, |ui| {
                                        for camera in self.cameras_for(item) {
                                            let channel = camera.channel;
                                            if ui
                                                .selectable_label(
                                                    self.playback_camera == Some((index, channel)),
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
                        self.playback_camera = Some(camera);
                    }
                    ui.separator();
                    ui.label("Início (hora do DVR)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.playback_start)
                            .hint_text("AAAA-MM-DD HH:MM:SS"),
                    );
                    ui.label("Fim (hora do DVR)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.playback_end)
                            .hint_text("AAAA-MM-DD HH:MM:SS"),
                    );
                    if ui.button("Reproduzir").clicked() {
                        if let Err(error) = self.connect_playback(ctx) {
                            self.status = error;
                        }
                    }
                    ui.small("Intelbras · H.264/H.265");
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                let available = ui.available_size();
                let spacing = ui.spacing().item_spacing;
                let width = ((available.x - spacing.x) / 2.0).max(1.0);
                let height = ((available.y - 42.0 - spacing.y) / 2.0).max(1.0);
                for row in 0..2 {
                    ui.horizontal(|ui| {
                        for col in 0..2 {
                            let index = row * 2 + col;
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(width, height),
                                egui::Sense::click(),
                            );
                            if response.clicked() {
                                self.playback_slot = index;
                            }
                            response.context_menu(|ui| {
                                if ui
                                    .add_enabled(
                                        self.playback_slots[index].camera.is_some(),
                                        egui::Button::new("Desconectar"),
                                    )
                                    .clicked()
                                {
                                    self.playback_slots[index].stop();
                                    self.status =
                                        format!("Reprodução do quadro {} encerrada.", index + 1);
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(
                                        self.playback_slots
                                            .iter()
                                            .any(|slot| slot.camera.is_some()),
                                        egui::Button::new("Fechar todas as câmeras"),
                                    )
                                    .clicked()
                                {
                                    for slot in &mut self.playback_slots {
                                        slot.stop();
                                    }
                                    self.status = "Reproduções encerradas.".into();
                                    ui.close_menu();
                                }
                            });
                            ui.painter().rect_filled(
                                rect,
                                0.0,
                                egui::Color32::from_rgb(20, 23, 26),
                            );
                            ui.painter().rect_stroke(
                                rect,
                                0.0,
                                egui::Stroke::new(
                                    if self.playback_slot == index {
                                        2.0_f32
                                    } else {
                                        1.0
                                    },
                                    if self.playback_slot == index {
                                        egui::Color32::from_rgb(35, 130, 255)
                                    } else {
                                        egui::Color32::GRAY
                                    },
                                ),
                                egui::StrokeKind::Inside,
                            );
                            if let Some(texture) = &self.playback_slots[index].texture {
                                let size = egui::vec2(rect.width(), rect.height()).min(egui::vec2(
                                    rect.height() * texture.size()[0] as f32
                                        / texture.size()[1] as f32,
                                    rect.width() * texture.size()[1] as f32
                                        / texture.size()[0] as f32,
                                ));
                                ui.painter().image(
                                    texture.id(),
                                    egui::Rect::from_center_size(rect.center(), size),
                                    egui::Rect::from_min_max(
                                        egui::Pos2::ZERO,
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                            } else {
                                let text = if self.playback_slots[index].camera.is_some() {
                                    "Conectando…"
                                } else {
                                    "Selecione uma câmera e um período"
                                };
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    text,
                                    egui::FontId::proportional(13.0),
                                    egui::Color32::GRAY,
                                );
                            }
                        }
                    });
                }
                ui.separator();
                ui.label(
                    "Linha do tempo indisponível: consulta das gravações ainda não autenticada.",
                );
            });
        });
    }

    fn set_grid_side(&mut self, side: usize) {
        debug_assert!((2..=4).contains(&side));
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
            name: self.name.trim().into(),
            brand: self.brand,
            host: self.host.clone(),
            port: self.port,
            http_port: self.http_port,
            username: self.username.clone(),
            channels: self.channel_count,
        };
        let mut updated = self.equipment.clone();
        if let Some(index) = self.selected {
            updated[index] = equipment;
        } else {
            updated.push(equipment);
        }
        match save_equipment(&updated) {
            Ok(()) => {
                self.selected = Some(if self.selected.is_some() {
                    self.selected.unwrap()
                } else {
                    updated.len() - 1
                });
                self.equipment = updated;
                if !self.password.is_empty() {
                    let id = &self.equipment[self.selected.unwrap()].id;
                    self.status = match credential(id).and_then(|entry| entry.set_password(&self.password)) {
                        Ok(()) => "Equipamento salvo com a senha no armazenamento seguro do sistema.".into(),
                        Err(_) => "Equipamento salvo, mas não foi possível salvar a senha no armazenamento seguro do sistema.".into(),
                    };
                } else {
                    self.status = "Equipamento salvo. Nenhuma nova senha foi informada.".into();
                }
            }
            Err(error) => self.status = error,
        }
    }

    fn select(&mut self, index: usize) {
        let equipment = &self.equipment[index];
        self.name = equipment.name.clone();
        self.brand = equipment.brand;
        self.host = equipment.host.clone();
        self.port = equipment.port;
        self.http_port = equipment.http_port;
        self.username = equipment.username.clone();
        self.channel_count = equipment.channels;
        self.password = match credential(&equipment.id).and_then(|entry| entry.get_password()) {
            Ok(password) => password,
            Err(keyring::Error::NoEntry) => String::new(),
            Err(_) => {
                self.status = "Não foi possível acessar a senha salva. Confira o armazenamento de credenciais do sistema.".into();
                String::new()
            }
        };
        self.selected = Some(index);
        self.channel = 1;
    }

    fn stop(&mut self) {
        for slot in &mut self.slots {
            slot.stop();
        }
    }

    fn connect(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let secondary = use_secondary(self.grid_side, self.expanded_slot, self.selected_slot);
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
        let credentials = retina::client::Credentials {
            username: self.username.clone(),
            password: self.password.clone(),
        };
        url.set_username("").map_err(|_| "URL inválida.")?;
        url.set_password(None).map_err(|_| "URL inválida.")?;
        let index = self.selected.ok_or("Selecione um equipamento.")?;
        self.slots[self.selected_slot].start(
            url,
            credentials,
            (self.equipment[index].id.clone(), self.channel),
            ctx,
        );
        self.slots[self.selected_slot].secondary = Some(secondary);
        self.status = "Conectando…".into();
        if self.expanded_slot.is_none() {
            self.selected_slot = next_slot(self.selected_slot, self.grid_side);
        }
        Ok(())
    }

    fn sync_live_quality(&mut self, ctx: &egui::Context) {
        for index in 0..self.grid_side * self.grid_side {
            let desired = use_secondary(self.grid_side, self.expanded_slot, index);
            let slot = &self.slots[index];
            if slot.secondary == Some(desired) || slot.camera.is_none() {
                continue;
            }
            let Some((id, channel)) = slot.camera.clone() else {
                continue;
            };
            let Some(equipment) = self.equipment.iter().find(|item| item.id == id) else {
                self.status =
                    "Não foi possível ajustar o fluxo: equipamento não encontrado.".into();
                continue;
            };
            let url = match camera_url(
                equipment.brand,
                &equipment.host,
                equipment.port,
                "",
                "",
                channel,
                desired,
            ) {
                Ok(url) => url,
                Err(error) => {
                    self.status = error.into();
                    continue;
                }
            };
            let password = match credential(&id).and_then(|entry| entry.get_password()) {
                Ok(password) => password,
                Err(_) => {
                    self.status = "Não foi possível acessar a senha para ajustar o fluxo.".into();
                    continue;
                }
            };
            let credentials = retina::client::Credentials {
                username: equipment.username.clone(),
                password,
            };
            self.slots[index].start(url, credentials, (id, channel), ctx);
            self.slots[index].secondary = Some(desired);
            self.status = "Ajustando a qualidade das câmeras…".into();
        }
    }

    fn connect_playback(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let (index, channel) = self.playback_camera.ok_or("Selecione uma câmera.")?;
        let equipment = &self.equipment[index];
        if equipment.brand != Brand::Intelbras {
            return Err("Reprodução disponível apenas para Intelbras nesta versão.".into());
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
        let url = playback::url(base, channel, &self.playback_start, &self.playback_end)?;
        let password = credential(&equipment.id)
            .and_then(|entry| entry.get_password())
            .map_err(|_| "Não foi possível acessar a senha salva do equipamento.")?;
        let credentials = retina::client::Credentials {
            username: equipment.username.clone(),
            password,
        };
        self.playback_slots[self.playback_slot].start(
            url,
            credentials,
            (equipment.id.clone(), channel),
            ctx,
        );
        self.status = format!(
            "Conectando reprodução no quadro {}…",
            self.playback_slot + 1
        );
        self.playback_slot = (self.playback_slot + 1) % self.playback_slots.len();
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

fn update_slots(slots: &mut [VideoSlot], ctx: &egui::Context, status: &mut String, label: &str) {
    for (index, slot) in slots.iter_mut().enumerate() {
        if let Some(receiver) = &slot.frames {
            if let Ok(event) = receiver.try_recv() {
                match event {
                    video::Event::Frame(frame) => {
                        let image =
                            egui::ColorImage::from_rgb([frame.width, frame.height], &frame.rgb);
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
                    video::Event::Status(message) => *status = message.into(),
                }
            }
        }
        if let Some(message) = slot
            .errors
            .as_ref()
            .and_then(|errors| errors.try_recv().ok())
        {
            *status = format!("{label}, quadro {}: {message}", index + 1);
            slot.stop();
        }
    }
}

impl eframe::App for Viewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("☰ Menu", |ui| {
                    if ui.button("Visualização ao vivo").clicked() {
                        self.live_tab_open = true;
                        self.tab = Tab::Live;
                        ui.close_menu();
                    }
                    if ui.button("Reprodução").clicked() {
                        self.playback_tab_open = true;
                        self.tab = Tab::Playback;
                        ui.close_menu();
                    }
                    if ui.button("Equipamentos").clicked() {
                        self.equipment_tab_open = true;
                        self.tab = Tab::Equipment;
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
                        for slot in &mut self.playback_slots {
                            slot.stop();
                        }
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
                ui.heading("Visor DVR");
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
                            ui.strong("Ação");
                            ui.end_row();
                            for (index, equipment) in self.equipment.iter().enumerate() {
                                ui.label(self.device_title(equipment))
                                    .on_hover_text(self.device_status(equipment));
                                ui.label(&equipment.host);
                                ui.label(match equipment.brand {
                                    Brand::Intelbras => "Intelbras",
                                    Brand::Hikvision => "Hikvision",
                                    Brand::Custom => "Personalizado",
                                });
                                ui.label(equipment.port.to_string());
                                let count = self.cameras_for(equipment).len();
                                ui.label(count.to_string())
                                    .on_hover_text(self.device_status(equipment));
                                if ui.button("Editar").clicked() {
                                    edit = Some(index);
                                }
                                ui.end_row();
                            }
                        });
                });
                if let Some(index) = edit {
                    self.select(index);
                    self.edit_open = true;
                }
            } else if self.tab == Tab::Playback {
                self.playback_ui(ui, ctx);
            } else {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(205.0, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.heading("Câmeras");
                            ui.separator();
                            let mut connect_camera = None;
                            let mut disconnect_camera = None;
                            let mut disconnect_all = false;
                            let list_height = (ui.available_height() - 60.0).max(0.0);
                            egui::ScrollArea::vertical()
                                .max_height(list_height)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for (index, item) in self.equipment.iter().enumerate() {
                                        egui::CollapsingHeader::new(self.device_title(item))
                                            .id_salt(("camera_tree", index))
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
                            ui.separator();
                            ui.horizontal(|ui| {
                                for side in 2..=4 {
                                    if ui
                                        .selectable_label(
                                            self.grid_side == side,
                                            format!("{}", side * side),
                                        )
                                        .on_hover_text(format!("{} quadro(s)", side * side))
                                        .clicked()
                                    {
                                        self.set_grid_side(side);
                                    }
                                }
                            });
                            ui.small("Qualidade ajustada à grade.");
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
                                    ui.painter().rect_stroke(
                                        rect,
                                        0.0,
                                        egui::Stroke::new(
                                            if self.selected_slot == index {
                                                2.0_f32
                                            } else {
                                                1.0
                                            },
                                            if self.selected_slot == index {
                                                egui::Color32::from_rgb(35, 130, 255)
                                            } else {
                                                egui::Color32::GRAY
                                            },
                                        ),
                                        egui::StrokeKind::Inside,
                                    );
                                    if self.slots[index].camera.is_some() {
                                        if let Some(texture) = &self.slots[index].texture {
                                            let size = egui::vec2(rect.width(), rect.height()).min(
                                                egui::vec2(
                                                    rect.height() * texture.size()[0] as f32
                                                        / texture.size()[1] as f32,
                                                    rect.width() * texture.size()[1] as f32
                                                        / texture.size()[0] as f32,
                                                ),
                                            );
                                            let image_rect =
                                                egui::Rect::from_center_size(rect.center(), size);
                                            ui.painter().image(
                                                texture.id(),
                                                image_rect,
                                                egui::Rect::from_min_max(
                                                    egui::Pos2::ZERO,
                                                    egui::pos2(1.0, 1.0),
                                                ),
                                                egui::Color32::WHITE,
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
                                }
                            });
                        }
                    });
                });
            }
        });
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
                                });
                            ui.end_row();
                            ui.label("IP / domínio");
                            ui.text_edit_singleline(&mut self.host);
                            ui.end_row();
                            ui.label("Porta RTSP");
                            ui.add(egui::DragValue::new(&mut self.port).range(1..=u16::MAX));
                            ui.end_row();
                            ui.label("Porta HTTP");
                            ui.add(egui::DragValue::new(&mut self.http_port).range(1..=u16::MAX));
                            ui.end_row();
                            ui.label("Usuário");
                            ui.text_edit_singleline(&mut self.username);
                            ui.end_row();
                            ui.label("Senha");
                            ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                            ui.end_row();
                            ui.label("Quantidade de canais");
                            ui.add(egui::DragValue::new(&mut self.channel_count).range(1..=256));
                            ui.end_row();
                        });
                    ui.label(
                        "A senha é salva no armazenamento de credenciais do sistema, não no JSON.",
                    );
                    ui.label("RTSP geralmente usa a porta 554, não a porta de serviço 37777.");
                    if ui.button("Salvar").clicked() {
                        self.save();
                        if !self.status.contains("não foi possível salvar a senha")
                            && self.status.starts_with("Equipamento salvo")
                        {
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
        let layout = (self.grid_side, self.expanded_slot);
        if layout != self.quality_layout {
            self.quality_layout = layout;
            self.sync_live_quality(ctx);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Visor DVR",
        options,
        Box::new(|_| Ok(Box::<Viewer>::default())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_stream_quality_follows_grid_and_expansion() {
        for index in 0..4 {
            assert!(!use_secondary(2, None, index));
        }
        for side in [3, 4] {
            for index in 0..side * side {
                assert!(use_secondary(side, None, index));
                assert_eq!(use_secondary(side, Some(5), index), index != 5);
            }
        }
        assert!(!use_secondary(2, Some(1), 1));
    }

    #[test]
    fn double_click_toggles_expansion_without_affecting_other_slots() {
        let mut slots: [VideoSlot; 16] = std::array::from_fn(|_| VideoSlot::default());
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
        for side in 1..=4 {
            let count = side * side;
            assert_eq!(next_slot(count - 1, side), 0);
            if count > 1 {
                assert_eq!(next_slot(0, side), 1);
            }
        }
    }

    #[test]
    fn reducing_grid_disconnects_only_hidden_slots() {
        let mut slots: [VideoSlot; 16] = std::array::from_fn(|_| VideoSlot::default());
        let (visible_tx, mut visible_rx) = tokio::sync::oneshot::channel();
        let (hidden_tx, mut hidden_rx) = tokio::sync::oneshot::channel();
        slots[3].camera = Some(("dvr".into(), 4));
        slots[3].cancel = Some(visible_tx);
        slots[4].camera = Some(("dvr".into(), 5));
        slots[4].cancel = Some(hidden_tx);
        let mut selected = 8;
        resize_grid(&mut slots, &mut selected, 2);
        assert_eq!(selected, 0);
        assert_eq!(hidden_rx.try_recv(), Ok(()));
        assert!(matches!(
            visible_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(slots[3].camera.is_some());
        assert!(slots[4].camera.is_none());
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
    fn old_equipment_loads_without_id_or_password_in_json() {
        let old = r#"{"name":"DVR","brand":"Intelbras","host":"dvr.local","port":554,"username":"admin","channels":4}"#;
        let equipment: Equipment = serde_json::from_str(old).unwrap();
        assert!(equipment.id.is_empty());
        let json = serde_json::to_string(&equipment).unwrap();
        assert!(!json.contains("password"));
    }
}
