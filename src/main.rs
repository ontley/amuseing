use std::time::Duration;

use amuseing::{player::Playlist, Player, Queue, RepeatMode};

use eframe::{run_native, App};
use egui::{Color32, Layout, Sense, ViewportBuilder, Widget};

struct PlaylistButton {
    playlist_name: String,
    selected: bool,
}

impl PlaylistButton {
    fn new(name: String) -> Self {
        Self {
            playlist_name: name,
            selected: false,
        }
    }

    fn selected(self, selected: bool) -> Self {
        Self { selected, ..self }
    }
}

impl Widget for PlaylistButton {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let color = if self.selected {
            Color32::GRAY
        } else {
            Color32::DARK_GRAY
        };
        let frame = egui::Frame::none().fill(color).rounding(5.0);
        frame
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.vertical(|ui| {
                    ui.horizontal_centered(|ui| ui.label(self.playlist_name));
                });
            })
            .response
    }
}

fn format_time(secs: &u64) -> String {
    let (mins, secs) = (secs / 60, secs % 60);
    let (hours, mins) = (mins / 60, mins % 60);
    if hours == 0 {
        format!("{mins:02}:{secs:02}")
    } else {
        format!("{hours:02}:{mins:02}:{secs:02}")
    }
}

struct PlayerControls<'a> {
    player: &'a mut Player,
    song: &'a amuseing::Song,
    volume: &'a mut f32,
}

impl<'a> PlayerControls<'a> {
    fn new(player: &'a mut Player, song: &'a amuseing::Song, volume: &'a mut f32) -> Self {
        Self {
            player,
            song,
            volume,
        }
    }
}

impl<'a> Widget for &mut PlayerControls<'a> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let duration_bar_height = ui.available_height() * 0.05;
        let duration_total = self.song.duration();
        let duration = self.player.duration();
        let duration_portion = duration.as_secs_f32() / duration_total.as_secs_f32();

        //
        // Render the duration bar
        //
        ui.vertical(|ui| {
            if self.player.is_playing() {
                if !ui.ctx().has_requested_repaint() {
                    ui.ctx().request_repaint_after(Duration::from_millis(10));
                }
            }
            let duration_frame = egui::Frame::none().fill(Color32::DARK_GRAY);
            let resp = duration_frame
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_height(duration_bar_height);
                    let colored_frame = egui::Frame::none().fill(Color32::YELLOW);
                    colored_frame.show(ui, |ui| {
                        let colored_width = ui.available_width() * duration_portion;
                        ui.set_width(colored_width);
                        ui.set_height(ui.available_height());
                    })
                })
                .response
                .interact(Sense::click_and_drag());

            if let Some(mouse_pos) = resp.interact_pointer_pos() {
                let mouse_x = mouse_pos.x;
                let rect_left = resp.rect.left();
                let rect_width = resp.rect.width();
                let portion = (mouse_x - rect_left) / rect_width;
                let duration = self.song.duration().mul_f32(portion);

                if resp.clicked() || resp.dragged() {
                    self.player
                        .seek_duration(duration)
                        .expect("Seeking from the bar should not fail");
                }
                // Show a tooltip of the duration to seek to
                if resp.hovered() {
                    resp.on_hover_ui_at_pointer(|ui| {
                        let text = format_time(&duration.as_secs());
                        ui.label(text);
                    });
                }
            }
        });

        ui.horizontal_centered(|ui| {
            //
            // Render the player buttons (rewind, pause/play, fast-forward)
            //
            let main_controls_width = ui.available_width() * 0.15;
            ui.allocate_ui(
                egui::vec2(main_controls_width, ui.available_height()),
                |ui| {
                    ui.set_height(ui.available_height() * 0.8);
                    ui.set_width(main_controls_width);
                    ui.columns(3, |columns| {
                        columns[0].centered_and_justified(|ui| {
                            if ui.button("<").clicked() {
                                self.player.rewind();
                            }
                        });
                        columns[1].centered_and_justified(|ui| {
                            let playing = self.player.is_playing();
                            let button_text = if playing { "||" } else { "|>" };
                            if ui.button(button_text).clicked() {
                                if playing {
                                    self.player.pause();
                                } else {
                                    self.player.resume();
                                }
                            }
                        });
                        columns[2].centered_and_justified(|ui| {
                            if ui.button(">").clicked() {
                                self.player.fast_forward();
                            }
                        })
                    })
                },
            );
            //
            // Shot a ratio of the time passed / duration of the song
            // e.g. 2:03 / 3:12
            //
            let formatted_passed = format_time(&duration.as_secs());
            let formatted_total = format_time(&duration_total.as_secs());
            let formatted_label = format!("{formatted_passed} / {formatted_total}");
            // let label_width = ui.available_width() * 0.05;
            ui.label(formatted_label);

            let slider = egui::Slider::new(self.volume, 0f32..=1f32).show_value(false);
            let resp = ui.add(slider);
            // This doesn't work properly I think, because only the "head" of the slider should be interacted with.
            // Or maybe it does.
            if resp.clicked() || resp.dragged() {
                self.player.set_volume(self.volume);
            };

            let mut queue_lock = self.player.queue().lock().unwrap();
            let next = match queue_lock.repeat_mode {
                RepeatMode::All => RepeatMode::Single,
                RepeatMode::Single => RepeatMode::Off,
                RepeatMode::Off => RepeatMode::All,
            };
            if ui.button("🔁").clicked() {
                queue_lock.repeat_mode = next;
            }
            drop(queue_lock);
        })
        .response
    }
}

struct SongWidget<'a> {
    playlist: &'a Playlist,
    player: &'a mut Player,
    song: &'a amuseing::Song,
    id: usize,
}

impl<'a> SongWidget<'a> {
    pub fn new(
        playlist: &'a Playlist,
        player: &'a mut Player,
        song: &'a amuseing::Song,
        id: usize,
    ) -> Self {
        Self {
            playlist,
            player,
            song,
            id,
        }
    }
}

impl<'a> Widget for &'a mut SongWidget<'a> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let frame = egui::Frame::none()
            .fill(Color32::from_gray(50))
            .inner_margin(egui::Margin::symmetric(10., 0.));
        frame
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_height(ui.available_height());
                ui.horizontal_centered(|ui| {
                    if ui.button("|>").clicked() {
                        self.playlist.play_from_index(self.player, self.id)
                    }
                    ui.label(self.song.title());
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format_time(&self.song.duration().as_secs()));
                    });
                });
            })
            .response
    }
}

struct AmuseingApp {
    playlists: Vec<Playlist>,
    current_playlist_index: usize,
    player: Player,
    volume: f32,
    songs: Vec<amuseing::Song>,
}

impl Default for AmuseingApp {
    fn default() -> Self {
        let playlists = vec![
            Playlist::new("Bruh 1".to_string(), "D:\\coding\\amuseing\\audio".into()).unwrap(),
            Playlist::new("Bruh 2".to_string(), "D:\\coding\\amuseing\\audio".into()).unwrap(),
        ];
        let volume = 1.;
        let current_playlist_index = 0;
        let songs = playlists[current_playlist_index].songs();
        let queue = Queue::new(Vec::new(), 0, RepeatMode::All);
        let mut player = Player::with_queue(queue);
        player.set_volume(&volume);
        Self {
            playlists,
            current_playlist_index,
            player,
            volume,
            songs,
        }
    }
}

impl App for AmuseingApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (window_width, window_height) = ctx.input(|i| {
            let viewport = i.viewport();
            let window_size = viewport.monitor_size.unwrap();
            (window_size.x, window_size.y)
        });

        let _song_panel_height_ratio = 0.12;
        let song_panel_height_min = window_height * 0.1;
        let _song_panel_height_max = window_height * 1.;

        let playlist_panel_width_ratio = 0.2;
        let playlist_panel_width_min = window_width * 0.1;
        let playlist_panel_width_max = window_width * 1.;

        let playlist_button_height_ratio = 0.15;
        let playlist_button_height_min = window_height * 0.055;
        let playlist_button_height_max = window_height * 1.;

        let song_widget_height_ratio = 0.1;
        let song_widget_height_min = window_height * 0.045;
        let song_widget_height_max = window_height * 1.;

        if let Some(song) = self.player.current() {
            // let song_panel_height = (content_size.height() * song_panel_height_ratio)
            //     .clamp(song_panel_min_height, song_panel_max_height);
            let song_panel_height = song_panel_height_min;
            let song_panel = egui::TopBottomPanel::bottom("Song panel")
                .exact_height(song_panel_height)
                .resizable(false);

            song_panel.show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.add(&mut PlayerControls::new(
                        &mut self.player,
                        &song,
                        &mut self.volume,
                    ));
                })
            });
        }

        let center_panel = egui::CentralPanel::default().frame(egui::Frame::none());
        center_panel.show(ctx, |ui| {
            let playlist_panel_width = (ui.available_size().x * playlist_panel_width_ratio)
                .clamp(playlist_panel_width_min, playlist_panel_width_max);

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
                        .clamp(playlist_button_height_min, playlist_button_height_max);
                    draw_playlists(
                        ui,
                        &self.playlists,
                        &mut self.current_playlist_index,
                        &mut self.songs,
                        size,
                    );
                });
            });
            let songs_panel = egui::CentralPanel::default().frame(egui::Frame::none());
            songs_panel.show_inside(ui, |ui| {
                if self.current_playlist_index < self.playlists.len() {
                    let playlist = &self.playlists[self.current_playlist_index];
                    let scroll_area = egui::ScrollArea::vertical().scroll_bar_visibility(
                        egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                    );
                    scroll_area.show(ui, |ui| {
                        let mut size = ui.available_size();
                        size.y = (size.y * song_widget_height_ratio)
                            .clamp(song_widget_height_min, song_widget_height_max);
                        draw_playlist_songs(&mut self.player, ui, playlist, &self.songs, size);
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        // TODO: Add button for creating new playlists or opening a folder
                        ui.label("You have no playlists created")
                    });
                }
            });
        });
    }
}

fn draw_playlist_songs(
    player: &mut Player,
    ui: &mut egui::Ui,
    playlist: &Playlist,
    songs: &[amuseing::Song],
    size: egui::Vec2,
) {
    for (id, song) in songs.iter().enumerate() {
        let song_widget = &mut SongWidget::new(playlist, player, song, id);
        ui.add_sized(size, song_widget);
    }
}

fn draw_playlists(
    ui: &mut egui::Ui,
    playlists: &[Playlist],
    selected_id: &mut usize,
    songs: &mut Vec<amuseing::Song>,
    size: egui::Vec2,
) {
    for (i, playlist) in playlists.iter().enumerate() {
        let playlist_button =
            PlaylistButton::new(playlist.name().to_string()).selected(*selected_id == i);
        if ui
            .add_sized(size, playlist_button)
            .interact(egui::Sense::click())
            .clicked()
        {
            *selected_id = i;
            *songs = playlist.songs();
        };
        if i != playlists.len() - 1 {
            // FIXME: separators don't increase in size, only increase margins
            let sep = egui::Separator::default().spacing(25.);
            ui.add(sep);
        }
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
