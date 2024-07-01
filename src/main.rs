use std::time::Duration;

use amuseing::{player::Playlist, Player, PlayerState, Queue, RepeatMode};

use eframe::{run_native, App};
use egui::{Color32, Sense, ViewportBuilder, Widget};

struct PlaylistButton {
    playlist_name: String,
}

impl PlaylistButton {
    fn new(name: String) -> Self {
        Self {
            playlist_name: name,
        }
    }
}

impl Widget for PlaylistButton {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let frame = egui::Frame::none();
        frame
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.horizontal_centered(|ui| ui.label(self.playlist_name))
            })
            .response
    }
}

struct PlayerControls<'a> {
    player: &'a mut Player,
}

impl<'a> PlayerControls<'a> {
    fn new(player: &'a mut Player) -> Self {
        Self { player }
    }
}

impl<'a> Widget for &mut PlayerControls<'a> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let duration_bar_height = ui.available_height() * 0.1;

        ui.vertical_centered(|ui| {
            let ctx = ui.ctx();
            if !ctx.has_requested_repaint() {
                ui.ctx().request_repaint_after(Duration::from_millis(10));
            }
            let song = self.player.current();
            let portion = song.as_ref().map_or(0.0, |song| {
                let duration_total = song.duration();
                let duration = self.player.duration();
                duration.as_secs_f32() / duration_total.as_secs_f32()
            });
            let duration_frame = egui::Frame::none().fill(Color32::DARK_GRAY);
            let resp = duration_frame
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.set_min_height(duration_bar_height);
                    ui.horizontal(|ui| {
                        let colored_frame = egui::Frame::none().fill(Color32::YELLOW);
                        colored_frame.show(ui, |ui| {
                            let colored_width = ui.available_width() * portion;
                            ui.set_width(colored_width);

                            // FIXME: This is a hack.
                            // The frame doesn't show up if there are no widgets inside it
                            ui.label("");
                        })
                    })
                })
                .response
                .interact(Sense::click_and_drag());
            if resp.clicked() || resp.dragged() {
                let mouse_x = resp
                    .interact_pointer_pos()
                    .expect("Should not be None if there is an interaction")
                    .x;
                let left = resp.rect.left();
                let width = resp.rect.width();
                let portion = (mouse_x - left) / width;
                self.player.seek_portion(portion);
            }

            ui.columns(3, |columns| {
                columns[0].vertical_centered(|ui| {
                    if ui.button("<").clicked() {
                        self.player.rewind();
                    }
                });
                columns[1].vertical_centered(|ui| {
                    let playing = match self.player.state() {
                        PlayerState::Playing => true,
                        _ => false,
                    };
                    let button_text = if playing { "||" } else { "|>" };
                    if ui.button(button_text).clicked() {
                        if playing {
                            self.player.pause();
                        } else {
                            self.player.resume();
                        }
                    }
                });
                columns[2].vertical_centered(|ui| {
                    if ui.button(">").clicked() {
                        self.player.fast_forward();
                    }
                })
            })
        })
        .response
    }
}

struct AmuseingApp {
    playlists: Vec<Playlist>,
    current_playlist_id: Option<usize>,
    player: Player,
}

impl Default for AmuseingApp {
    fn default() -> Self {
        let playlist = Playlist::new("Bruh".to_string(), "D:\\coding\\amuseing\\audio".into());
        let songs = playlist.songs();
        println!("{:?}", songs);
        let queue = Queue::new(songs, 0, RepeatMode::All);
        let player = Player::with_queue(queue);
        Self {
            playlists: Vec::new(),
            current_playlist_id: None,
            player,
        }
    }
}

impl App for AmuseingApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let (monitor_size, content_size) = ctx.input(|i| {
            let viewport = i.viewport();
            (viewport.monitor_size.unwrap(), viewport.inner_rect.unwrap())
        });

        let song_panel_height_ratio = 0.12;
        let song_panel_min_height = monitor_size.x * 0.05;
        let song_panel_max_height = monitor_size.x * 1.;

        let playlist_panel_width_ratio = 0.2;
        let playlist_panel_min_width = monitor_size.x * 0.1;
        let playlist_panel_max_width = monitor_size.x * 1.;

        let playlist_button_height_ratio = 0.15;
        let min_playlist_button_height = monitor_size.y * 0.075;
        let max_playlist_button_height = monitor_size.y * 1.;

        // let song_panel_height = (content_size.height() * song_panel_height_ratio)
        //     .clamp(song_panel_min_height, song_panel_max_height);
        let song_panel_height = song_panel_min_height;
        let song_panel = egui::TopBottomPanel::bottom("Song panel")
            .exact_height(song_panel_height)
            .resizable(false);

        song_panel.show(ctx, |ui| {
            // TODO: progress bar after adding total duration to `Song`
            ui.centered_and_justified(|ui| {
                ui.add(&mut PlayerControls::new(&mut self.player));
            })
        });

        let center_panel = egui::CentralPanel::default();
        center_panel.show(ctx, |ui| {
            let playlist_panel_width = (ui.available_size().x * playlist_panel_width_ratio)
                .clamp(playlist_panel_min_width, playlist_panel_max_width);

            let playlist_panel = egui::SidePanel::left("Playlist panel")
                .exact_width(playlist_panel_width)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::none().fill(Color32::RED));

            playlist_panel.show_inside(ui, |ui| {
                let scroll_area = egui::ScrollArea::vertical()
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);

                scroll_area.show(ui, |ui| {
                    let mut size = ui.available_size();
                    size.y = (size.y * playlist_button_height_ratio)
                        .clamp(min_playlist_button_height, max_playlist_button_height);
                    for (i, playlist) in self.playlists.iter().enumerate() {
                        let playlist_button = PlaylistButton::new(playlist.name().to_string());
                        if ui
                            .add_sized(size, playlist_button)
                            .interact(egui::Sense::click())
                            .clicked()
                        {
                            self.current_playlist_id = Some(i);
                        };
                        if i != self.playlists.len() - 1 {
                            let sep = egui::Separator::default().spacing(25.);
                            ui.add(sep);
                        }
                    }
                })
            });
            ui.centered_and_justified(|ui| {
                if let Some(playlist_id) = self.current_playlist_id {
                    let playlist = &self.playlists[playlist_id];
                    ui.label(playlist.name());
                } else {
                    if ui.button("Click me to start").clicked() {
                        self.player.run();
                    }
                }
            })
        });
    }
}

fn main() {
    let mut native_options = eframe::NativeOptions::default();
    native_options.viewport = ViewportBuilder::default()
        .with_title("Amuseing")
        .with_inner_size((1200., 675.))
        .with_min_inner_size((600., 360.));
    let app = AmuseingApp::default();
    run_native("Amuseing", native_options, Box::new(|_cc| Box::new(app))).unwrap();
}
