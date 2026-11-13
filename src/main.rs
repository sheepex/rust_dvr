use eframe::egui;
mod video;
use serde::{Deserialize, Serialize};
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
    username: String,
    channels: u32,
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

struct Viewer {
    tab: Tab,
    equipment_tab_open: bool,
    live_tab_open: bool,
    edit_open: bool,
    selected_slot: usize,
    video_slot: Option<usize>,
    camera_filter: String,
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
    secondary: bool,
    custom_url: String,
    frames: Option<Receiver<video::Event>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    texture: Option<egui::TextureHandle>,
    status: String,
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
        Self {
            tab: Tab::Equipment,
            equipment_tab_open: true,
            live_tab_open: false,
            edit_open: false,
            selected_slot: 0,
            video_slot: None,
            camera_filter: String::new(),
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
            secondary: true,
            custom_url: String::new(),
            frames: None,
            cancel: None,
            texture: None,
            status,
        }
    }
}

impl Viewer {
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
        self.frames = None;
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        self.texture = None;
        self.video_slot = None;
    }

    fn connect(&mut self, ctx: &egui::Context) -> Result<(), String> {
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
                self.secondary,
            )?
        };
        let mut url = url;
        let credentials = retina::client::Credentials {
            username: self.username.clone(),
            password: self.password.clone(),
        };
        url.set_username("").map_err(|_| "URL inválida.")?;
        url.set_password(None).map_err(|_| "URL inválida.")?;
        self.stop();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                tokio::select! {
                    _ = cancel_rx => {},
                    result = video::run(url, credentials, sender.clone(), Some(ctx.clone())) => {
                        if let Err(error) = result { let _ = sender.try_send(video::Event::Status(error)); ctx.request_repaint(); }
                    }
                }
            });
        });
        self.frames = Some(receiver);
        self.cancel = Some(cancel_tx);
        self.video_slot = Some(self.selected_slot);
        self.status = "Conectando…".into();
        Ok(())
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl eframe::App for Viewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(receiver) = &self.frames {
            if let Ok(event) = receiver.try_recv() {
                match event {
                    video::Event::Frame(frame) => {
                        let image =
                            egui::ColorImage::from_rgb([frame.width, frame.height], &frame.rgb);
                        if let Some(texture) = &mut self.texture {
                            texture.set(image, egui::TextureOptions::LINEAR);
                        } else {
                            self.texture = Some(ctx.load_texture(
                                "camera",
                                image,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                        self.status = "Recebendo vídeo.".into();
                    }
                    video::Event::Status(message) => self.status = message.into(),
                }
            }
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("☰ Menu", |ui| {
                    if ui.button("Equipamentos").clicked() {
                        self.equipment_tab_open = true;
                        self.tab = Tab::Equipment;
                        ui.close_menu();
                    }
                    if ui.button("Visualização ao vivo").clicked() {
                        self.live_tab_open = true;
                        self.tab = Tab::Live;
                        if let Some(index) = self.selected {
                            self.select(index);
                        }
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
                        if self.tab == Tab::Equipment && self.live_tab_open {
                            self.tab = Tab::Live;
                        }
                    }
                }
                if self.live_tab_open {
                    if ui
                        .selectable_label(self.tab == Tab::Live, "Visualização ao vivo")
                        .clicked()
                    {
                        self.tab = Tab::Live;
                        if let Some(index) = self.selected {
                            self.select(index);
                        }
                    }
                    if ui
                        .small_button("×")
                        .on_hover_text("Fechar aba de visualização")
                        .clicked()
                    {
                        self.live_tab_open = false;
                        self.stop();
                        if self.tab == Tab::Live && self.equipment_tab_open {
                            self.tab = Tab::Equipment;
                        }
                    }
                }
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.equipment_tab_open && !self.live_tab_open {
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
                        self.channel_count = 4;
                        self.edit_open = true;
                    }
                    ui.label(format!(
                        "Equipamentos cadastrados: {}",
                        self.equipment.len()
                    ));
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
                                ui.label(&equipment.name);
                                ui.label(&equipment.host);
                                ui.label(match equipment.brand {
                                    Brand::Intelbras => "Intelbras",
                                    Brand::Hikvision => "Hikvision",
                                    Brand::Custom => "Personalizado",
                                });
                                ui.label(equipment.port.to_string());
                                ui.label(equipment.channels.to_string());
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
            } else {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(205.0, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.heading("Câmeras");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.camera_filter)
                                    .hint_text("Buscar equipamento"),
                            );
                            ui.separator();
                            let mut selection = None;
                            egui::ScrollArea::vertical()
                                .max_height(250.0)
                                .show(ui, |ui| {
                                    for (index, item) in self.equipment.iter().enumerate() {
                                        if !item
                                            .name
                                            .to_lowercase()
                                            .contains(&self.camera_filter.to_lowercase())
                                        {
                                            continue;
                                        }
                                        egui::CollapsingHeader::new(&item.name)
                                            .id_salt(("camera_tree", index))
                                            .show(ui, |ui| {
                                                for channel in 1..=item.channels {
                                                    if ui
                                                        .selectable_label(
                                                            self.selected == Some(index)
                                                                && self.channel == channel,
                                                            format!("Câmera {channel}"),
                                                        )
                                                        .clicked()
                                                    {
                                                        selection = Some((index, channel));
                                                    }
                                                }
                                            });
                                    }
                                });
                            if let Some((index, channel)) = selection {
                                if self.selected != Some(index) {
                                    self.select(index);
                                }
                                self.channel = channel;
                            }
                            ui.separator();
                            if self.selected.is_some() {
                                ui.label(format!("Câmera selecionada: {}", self.channel));
                                ui.label("Senha");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.password).password(true),
                                );
                                if ui.button("Salvar senha").clicked() {
                                    if self.password.is_empty() {
                                        self.status = "Informe uma senha para salvar.".into();
                                    } else if let Some(index) = self.selected {
                                        self.status = match credential(&self.equipment[index].id)
                                            .and_then(|entry| entry.set_password(&self.password))
                                        {
                                            Ok(()) => "Senha salva no armazenamento seguro do sistema.".into(),
                                            Err(_) => "Não foi possível salvar a senha no armazenamento seguro do sistema.".into(),
                                        };
                                    }
                                }
                                ui.checkbox(&mut self.secondary, "Fluxo secundário");
                                if ui.button("Visualizar câmera").clicked() {
                                    if let Err(error) = self.connect(ctx) {
                                        self.status = error;
                                    }
                                }
                                if ui.button("Desconectar").clicked() {
                                    self.stop();
                                    self.status = "Desconectado.".into();
                                }
                            } else {
                                ui.label("Escolha uma câmera na lista.");
                            }
                            ui.separator();
                            ui.label(&self.status);
                            ui.small("Por enquanto, apenas uma câmera simultânea.");
                        },
                    );
                    ui.separator();
                    let available = ui.available_size();
                    let width = (available.x / 2.0 - 4.0).max(20.0);
                    let height = (available.y / 2.0 - 4.0).max(20.0);
                    ui.vertical(|ui| {
                        for row in 0..2 {
                            ui.horizontal(|ui| {
                                for col in 0..2 {
                                    let index = row * 2 + col;
                                    let (rect, response) = ui.allocate_exact_size(
                                        egui::vec2(width, height),
                                        egui::Sense::click(),
                                    );
                                    if response.clicked() {
                                        self.selected_slot = index;
                                    }
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
                                    if self.video_slot == Some(index) {
                                        if let Some(texture) = &self.texture {
                                            let size = egui::vec2(rect.width(), rect.height()).min(
                                                egui::vec2(
                                                    rect.height() * texture.size()[0] as f32 / texture.size()[1] as f32,
                                                    rect.width() * texture.size()[1] as f32 / texture.size()[0] as f32,
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
                        }
                    }
                    ui.label(&self.status);
                });
            self.edit_open &= open;
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
