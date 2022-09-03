use fluent_templates::Loader;
use rand::RngCore;
use sha3::digest::{ExtendableOutput, Update};
use subtle::ConstantTimeEq;

use crate::{audio, config, game, gui, i18n, net, patch, rom, save, session, stats};

struct LobbySelection {
    pub game: &'static (dyn game::Game + Send + Sync),
    pub save: Box<dyn save::Save + Send + Sync>,
    pub rom: Vec<u8>,
    pub patch: Option<(String, semver::Version, patch::Version)>,
}

struct Lobby {
    attention_requested: bool,
    sender: Option<net::Sender>,
    selection: Option<LobbySelection>,
    nickname: String,
    match_type: (u8, u8),
    reveal_setup: bool,
    remote_rom: Option<Vec<u8>>,
    remote_settings: net::protocol::Settings,
    remote_commitment: Option<[u8; 16]>,
    latencies: stats::DeltaCounter,
    local_negotiated_state: Option<(net::protocol::NegotiatedState, Vec<u8>)>,
    roms_scanner: gui::ROMsScanner,
    patches_scanner: gui::PatchesScanner,
}

fn are_settings_compatible(
    local_settings: &net::protocol::Settings,
    remote_settings: &net::protocol::Settings,
    roms: &std::collections::HashMap<&'static (dyn game::Game + Send + Sync), Vec<u8>>,
    patches: &std::collections::BTreeMap<String, patch::Patch>,
) -> bool {
    let local_game_info = if let Some(gi) = local_settings.game_info.as_ref() {
        gi
    } else {
        return false;
    };

    let remote_game_info = if let Some(gi) = remote_settings.game_info.as_ref() {
        gi
    } else {
        return false;
    };

    if !remote_settings
        .available_games
        .iter()
        .any(|g| g == &local_game_info.family_and_variant)
    {
        return false;
    }

    if !local_settings
        .available_games
        .iter()
        .any(|g| g == &remote_game_info.family_and_variant)
    {
        return false;
    }

    if let Some(patch) = local_game_info.patch.as_ref() {
        if !remote_settings
            .available_patches
            .iter()
            .any(|(pn, pvs)| pn == &patch.name && pvs.contains(&patch.version))
        {
            return false;
        }
    }

    if let Some(patch) = remote_game_info.patch.as_ref() {
        if !local_settings
            .available_patches
            .iter()
            .any(|(pn, pvs)| pn == &patch.name && pvs.contains(&patch.version))
        {
            return false;
        }
    }

    #[derive(PartialEq)]
    struct SimplifiedSettings {
        netplay_compatibility: Option<String>,
        match_type: (u8, u8),
    }

    impl SimplifiedSettings {
        fn new(
            settings: &net::protocol::Settings,
            roms: &std::collections::HashMap<&'static (dyn game::Game + Send + Sync), Vec<u8>>,
            patches: &std::collections::BTreeMap<String, patch::Patch>,
        ) -> Self {
            Self {
                netplay_compatibility: settings.game_info.as_ref().and_then(|g| {
                    if game::find_by_family_and_variant(
                        g.family_and_variant.0.as_str(),
                        g.family_and_variant.1,
                    )
                    .map(|g| roms.contains_key(&g))
                    .unwrap_or(false)
                    {
                        if let Some(patch) = g.patch.as_ref() {
                            patches.get(&patch.name).and_then(|p| {
                                p.versions
                                    .get(&patch.version)
                                    .map(|vinfo| vinfo.netplay_compatibility.clone())
                            })
                        } else {
                            Some(g.family_and_variant.0.clone())
                        }
                    } else {
                        None
                    }
                }),
                match_type: settings.match_type,
            }
        }
    }

    let local_simplified_settings = SimplifiedSettings::new(&local_settings, roms, patches);
    let remote_simplified_settings = SimplifiedSettings::new(&remote_settings, roms, patches);

    local_simplified_settings.netplay_compatibility.is_some()
        && remote_simplified_settings.netplay_compatibility.is_some()
        && local_simplified_settings == remote_simplified_settings
}

fn make_commitment(buf: &[u8]) -> [u8; 16] {
    let mut shake128 = sha3::Shake128::default();
    shake128.update(b"tango:lobby:");
    shake128.update(buf);
    let mut commitment = [0u8; 16];
    shake128.finalize_xof_into(&mut commitment);
    commitment
}

impl Lobby {
    async fn uncommit(&mut self) -> Result<(), anyhow::Error> {
        let sender = if let Some(sender) = self.sender.as_mut() {
            sender
        } else {
            anyhow::bail!("no sender?")
        };

        sender.send_uncommit().await?;
        self.local_negotiated_state = None;
        Ok(())
    }

    async fn commit(&mut self, save_data: &[u8]) -> Result<(), anyhow::Error> {
        let mut nonce = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce);
        let negotiated_state = net::protocol::NegotiatedState {
            nonce: nonce.clone(),
            save_data: save_data.to_vec(),
        };
        let buf = zstd::stream::encode_all(
            &net::protocol::NegotiatedState::serialize(&negotiated_state).unwrap()[..],
            0,
        )?;
        let commitment = make_commitment(&buf);

        log::info!("nonce = {:02x?}, commitment = {:02x?}", nonce, commitment);

        let sender = if let Some(sender) = self.sender.as_mut() {
            sender
        } else {
            anyhow::bail!("no sender?")
        };
        sender.send_commit(commitment).await?;
        self.local_negotiated_state = Some((negotiated_state, buf));
        Ok(())
    }

    fn make_local_settings(&self) -> net::protocol::Settings {
        let roms = self.roms_scanner.read();
        let patches = self.patches_scanner.read();

        net::protocol::Settings {
            nickname: self.nickname.clone(),
            match_type: self.match_type,
            game_info: self.selection.as_ref().map(|selection| {
                let (family, variant) = selection.game.family_and_variant();
                net::protocol::GameInfo {
                    family_and_variant: (family.to_string(), variant),
                    patch: selection.patch.as_ref().map(|(name, version, _)| {
                        net::protocol::PatchInfo {
                            name: name.clone(),
                            version: version.clone(),
                        }
                    }),
                }
            }),
            available_games: roms
                .keys()
                .map(|g| {
                    let (family, variant) = g.family_and_variant();
                    (family.to_string(), variant)
                })
                .collect(),
            available_patches: patches
                .iter()
                .map(|(p, info)| (p.clone(), info.versions.keys().cloned().collect()))
                .collect(),
            reveal_setup: self.reveal_setup,
        }
    }

    async fn send_settings(
        &mut self,
        settings: net::protocol::Settings,
    ) -> Result<(), anyhow::Error> {
        let sender = if let Some(sender) = self.sender.as_mut() {
            sender
        } else {
            anyhow::bail!("no sender?")
        };
        sender.send_settings(settings).await?;
        Ok(())
    }

    async fn set_reveal_setup(&mut self, reveal_setup: bool) -> Result<(), anyhow::Error> {
        if reveal_setup == self.reveal_setup {
            return Ok(());
        }
        self.send_settings(net::protocol::Settings {
            reveal_setup,
            ..self.make_local_settings()
        })
        .await?;
        self.reveal_setup = reveal_setup;
        if !self.reveal_setup {
            self.remote_commitment = None;
        }
        Ok(())
    }

    async fn set_match_type(&mut self, match_type: (u8, u8)) -> Result<(), anyhow::Error> {
        if match_type == self.match_type {
            return Ok(());
        }
        self.send_settings(net::protocol::Settings {
            match_type,
            ..self.make_local_settings()
        })
        .await?;
        self.match_type = match_type;
        Ok(())
    }

    fn set_remote_settings(
        &mut self,
        settings: net::protocol::Settings,
        patches_path: &std::path::Path,
    ) {
        let roms = self.roms_scanner.read();
        let patches = self.patches_scanner.read();

        let old_reveal_setup = self.remote_settings.reveal_setup;
        self.remote_rom = settings.game_info.as_ref().and_then(|gi| {
            game::find_by_family_and_variant(&gi.family_and_variant.0, gi.family_and_variant.1)
                .and_then(|game| {
                    roms.get(&game).and_then(|rom| {
                        if let Some(pi) = gi.patch.as_ref() {
                            let (rom_code, revision) = game.rom_code_and_revision();

                            let bps = match std::fs::read(
                                patches_path
                                    .join(&pi.name)
                                    .join(format!("v{}", pi.version))
                                    .join(format!(
                                        "{}_{:02}.bps",
                                        std::str::from_utf8(rom_code).unwrap(),
                                        revision
                                    )),
                            ) {
                                Ok(bps) => bps,
                                Err(e) => {
                                    log::error!(
                                        "failed to load patch {} to {:?}: {:?}",
                                        pi.name,
                                        (rom_code, revision),
                                        e
                                    );
                                    return None;
                                }
                            };

                            let rom = match patch::bps::apply(&rom, &bps) {
                                Ok(r) => r.to_vec(),
                                Err(e) => {
                                    log::error!(
                                        "failed to apply patch {} to {:?}: {:?}",
                                        pi.name,
                                        (rom_code, revision),
                                        e
                                    );
                                    return None;
                                }
                            };

                            Some(rom)
                        } else {
                            Some(rom.clone())
                        }
                    })
                })
        });

        self.remote_settings = settings;
        if !are_settings_compatible(
            &self.make_local_settings(),
            &self.remote_settings,
            &roms,
            &patches,
        ) || (old_reveal_setup && !self.remote_settings.reveal_setup)
        {
            self.local_negotiated_state = None;
        }
    }

    async fn send_pong(&mut self, ts: std::time::SystemTime) -> Result<(), anyhow::Error> {
        let sender = if let Some(sender) = self.sender.as_mut() {
            sender
        } else {
            anyhow::bail!("no sender?")
        };
        sender.send_pong(ts).await?;
        Ok(())
    }

    async fn send_ping(&mut self) -> Result<(), anyhow::Error> {
        let sender = if let Some(sender) = self.sender.as_mut() {
            sender
        } else {
            anyhow::bail!("no sender?")
        };
        sender.send_ping(std::time::SystemTime::now()).await?;
        Ok(())
    }
}

async fn run_connection_task(
    config: std::sync::Arc<parking_lot::RwLock<config::Config>>,
    handle: tokio::runtime::Handle,
    egui_ctx: egui::Context,
    audio_binder: audio::LateBinder,
    emu_tps_counter: std::sync::Arc<parking_lot::Mutex<stats::Counter>>,
    session: std::sync::Arc<parking_lot::Mutex<Option<session::Session>>>,
    selection: std::sync::Arc<parking_lot::Mutex<Option<gui::Selection>>>,
    roms_scanner: gui::ROMsScanner,
    patches_scanner: gui::PatchesScanner,
    matchmaking_addr: String,
    link_code: String,
    nickname: String,
    patches_path: std::path::PathBuf,
    replays_path: std::path::PathBuf,
    connection_task: std::sync::Arc<tokio::sync::Mutex<Option<ConnectionTask>>>,
    cancellation_token: tokio_util::sync::CancellationToken,
) {
    if let Err(e) = {
        let connection_task = connection_task.clone();

        tokio::select! {
            r = {
                let connection_task = connection_task.clone();
                let cancellation_token = cancellation_token.clone();
                (move || async move {
                    *connection_task.lock().await =
                        Some(ConnectionTask::InProgress {
                            state: ConnectionState::Signaling,
                            cancellation_token:
                                cancellation_token.clone(),
                        });
                    const OPEN_TIMEOUT: std::time::Duration =
                        std::time::Duration::from_secs(30);
                    let pending_conn = tokio::time::timeout(
                        OPEN_TIMEOUT,
                        net::signaling::open(
                            &matchmaking_addr,
                            &link_code,
                        ),
                    )
                    .await??;

                    *connection_task.lock().await =
                        Some(ConnectionTask::InProgress {
                            state: ConnectionState::Waiting,
                            cancellation_token:
                                cancellation_token.clone(),
                        });

                    let (dc, peer_conn) = pending_conn.connect().await?;
                    let (dc_tx, dc_rx) = dc.split();
                    let mut sender = net::Sender::new(dc_tx);
                    let mut receiver = net::Receiver::new(dc_rx);
                    net::negotiate(&mut sender, &mut receiver).await?;

                    let default_match_type = {
                        let config = config.read();
                        config.default_match_type
                    };

                    let lobby = std::sync::Arc::new(tokio::sync::Mutex::new(Lobby{
                        attention_requested: false,
                        sender: Some(sender),
                        selection: None,
                        nickname,
                        match_type: (if selection.lock().as_ref().map(|selection| (default_match_type as usize) < selection.game.match_types().len()).unwrap_or(false) {
                            default_match_type
                        } else {
                            0
                        }, 0),
                        reveal_setup: false,
                        remote_rom: None,
                        remote_settings: net::protocol::Settings::default(),
                        remote_commitment: None,
                        latencies: stats::DeltaCounter::new(10),
                        local_negotiated_state: None,
                        roms_scanner: roms_scanner.clone(),
                        patches_scanner: patches_scanner.clone(),
                    }));
                    {
                        let mut lobby = lobby.lock().await;
                        let settings = lobby.make_local_settings();
                        lobby.send_settings(settings).await?;
                    }

                    *connection_task.lock().await =
                        Some(ConnectionTask::InProgress {
                            state: ConnectionState::InLobby(lobby.clone()),
                            cancellation_token:
                                cancellation_token.clone(),
                        });

                    let mut remote_chunks = vec![];
                    let mut ping_timer = tokio::time::interval(net::PING_INTERVAL);
                    'l: loop {
                        tokio::select! {
                            _ = ping_timer.tick() => {
                                lobby.lock().await.send_ping().await?;
                            }
                            p = receiver.receive() => {
                                match p? {
                                    net::protocol::Packet::Ping(ping) => {
                                        lobby.lock().await.send_pong(ping.ts).await?;
                                    },
                                    net::protocol::Packet::Pong(pong) => {
                                        let mut lobby = lobby.lock().await;
                                        if let Ok(d) = std::time::SystemTime::now().duration_since(pong.ts) {
                                            lobby.latencies.mark(d);
                                            egui_ctx.request_repaint();
                                        }
                                    },
                                    net::protocol::Packet::Settings(settings) => {
                                        let mut lobby = lobby.lock().await;
                                        lobby.set_remote_settings(settings, &patches_path);
                                        egui_ctx.request_repaint();
                                    },
                                    net::protocol::Packet::Commit(commit) => {
                                        let mut lobby = lobby.lock().await;
                                        lobby.remote_commitment = Some(commit.commitment);
                                        egui_ctx.request_repaint();

                                        if lobby.local_negotiated_state.is_some() {
                                            break 'l;
                                        }
                                    },
                                    net::protocol::Packet::Uncommit(_) => {
                                        lobby.lock().await.remote_commitment = None;
                                        egui_ctx.request_repaint();
                                    },
                                    net::protocol::Packet::Chunk(chunk) => {
                                        remote_chunks.push(chunk.chunk);
                                        break 'l;
                                    },
                                    p => {
                                        anyhow::bail!("unexpected packet: {:?}", p);
                                    }
                                }
                            }
                        }
                    }

                    log::info!("ending lobby");

                    let (mut sender, match_type, local_settings, mut remote_rom, remote_settings, remote_commitment, local_negotiated_state) = {
                        let mut lobby = lobby.lock().await;
                        let local_settings = lobby.make_local_settings();
                        let sender = if let Some(sender) = lobby.sender.take() {
                            sender
                        } else {
                            anyhow::bail!("no sender?");
                        };
                        (sender, lobby.match_type, local_settings, lobby.remote_rom.clone(), lobby.remote_settings.clone(), lobby.remote_commitment.clone(), lobby.local_negotiated_state.take())
                    };

                    let remote_rom = if let Some(remote_rom) = remote_rom.take() {
                        remote_rom
                    } else {
                        anyhow::bail!("missing shadow rom");
                    };

                    let (local_negotiated_state, raw_local_state) = if let Some((negotiated_state, raw_local_state)) = local_negotiated_state {
                        (negotiated_state, raw_local_state)
                    } else {
                        anyhow::bail!("attempted to start match in invalid state");
                    };

                    const CHUNK_SIZE: usize = 32 * 1024;
                    const CHUNKS_REQUIRED: usize = 5;
                    for (_, chunk) in std::iter::zip(
                        0..CHUNKS_REQUIRED,
                        raw_local_state.chunks(CHUNK_SIZE).chain(std::iter::repeat(&[][..]))
                     ) {
                        sender.send_chunk(chunk.to_vec()).await?;

                        if remote_chunks.len() < CHUNKS_REQUIRED {
                            loop {
                                match receiver.receive().await? {
                                    net::protocol::Packet::Ping(ping) => {
                                        sender.send_pong(ping.ts).await?;
                                    },
                                    net::protocol::Packet::Pong(_) => { },
                                    net::protocol::Packet::Chunk(chunk) => {
                                        remote_chunks.push(chunk.chunk);
                                        break;
                                    },
                                    p => {
                                        anyhow::bail!("unexpected packet: {:?}", p);
                                    }
                                }
                            }
                        }
                    }

                    let raw_remote_negotiated_state = remote_chunks.into_iter().flatten().collect::<Vec<_>>();

                    let received_remote_commitment = if let Some(commitment) = remote_commitment {
                        commitment
                    } else {
                        anyhow::bail!("no remote commitment?");
                    };

                    log::info!("remote commitment = {:02x?}", received_remote_commitment);

                    if !bool::from(make_commitment(&raw_remote_negotiated_state).ct_eq(&received_remote_commitment)) {
                        anyhow::bail!("commitment did not match");
                    }

                    let remote_negotiated_state = zstd::stream::decode_all(&raw_remote_negotiated_state[..]).map_err(|e| e.into()).and_then(|r| net::protocol::NegotiatedState::deserialize(&r))?;

                    let rng_seed = std::iter::zip(local_negotiated_state.nonce, remote_negotiated_state.nonce).map(|(x, y)| x ^ y).collect::<Vec<_>>().try_into().unwrap();
                    log::info!("session verified! rng seed = {:02x?}", rng_seed);

                    let (local_game, local_rom, patch) = if let Some(selection) = selection.lock().as_ref() {
                        (selection.game, selection.rom.clone(), selection.patch.clone())
                    } else {
                        anyhow::bail!("attempted to start match in invalid state");
                    };

                    sender.send_start_match().await?;
                    match receiver.receive().await? {
                        net::protocol::Packet::StartMatch(_) => {},
                        p => anyhow::bail!("unexpected packet when expecting start match: {:?}", p),
                    }

                    log::info!("starting session");
                    let is_offerer = peer_conn.local_description().unwrap().sdp_type == datachannel_wrapper::SdpType::Offer;
                    {
                        *session.lock() = Some(session::Session::new_pvp(
                            config.clone(),
                            handle,
                            audio_binder,
                            link_code,
                            patch
                                .map(|(_, _, metadata)| metadata.netplay_compatibility.clone())
                                .unwrap_or(local_game.family_and_variant().0.to_owned()),
                            local_settings,
                            local_game,
                            &local_rom,
                            &local_negotiated_state.save_data,
                            remote_settings,
                            &remote_rom,
                            &remote_negotiated_state.save_data,
                            emu_tps_counter.clone(),
                            sender,
                            receiver,
                            peer_conn,
                            is_offerer,
                            replays_path,
                            match_type,
                            rng_seed,
                        )?);
                    }
                    egui_ctx.request_repaint();
                    *connection_task.lock().await = None;

                    Ok(())
                })(
                )
            }
            => {
                r
            }
            _ = cancellation_token.cancelled() => {
                Ok(())
            }
        }
    } {
        log::info!("connection task failed: {:?}", e);
        *connection_task.lock().await = Some(ConnectionTask::Failed(e));
    } else {
        *connection_task.lock().await = None;
    }
}

enum ConnectionTask {
    InProgress {
        state: ConnectionState,
        cancellation_token: tokio_util::sync::CancellationToken,
    },
    Failed(anyhow::Error),
}

enum ConnectionState {
    Starting,
    Signaling,
    Waiting,
    InLobby(std::sync::Arc<tokio::sync::Mutex<Lobby>>),
}

pub struct State {
    link_code: String,
    connection_task: std::sync::Arc<tokio::sync::Mutex<Option<ConnectionTask>>>,
    show_save_select: Option<gui::save_select_window::State>,
}

impl State {
    pub fn new() -> Self {
        Self {
            link_code: String::new(),
            connection_task: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            show_save_select: None,
        }
    }
}

pub struct PlayPane {
    save_view: gui::save_view::SaveView,
    save_select_window: gui::save_select_window::SaveSelectWindow,
}

impl PlayPane {
    pub fn new() -> Self {
        Self {
            save_view: gui::save_view::SaveView::new(),
            save_select_window: gui::save_select_window::SaveSelectWindow::new(),
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        handle: tokio::runtime::Handle,
        selection: std::sync::Arc<parking_lot::Mutex<Option<gui::Selection>>>,
        font_families: &gui::FontFamilies,
        clipboard: &mut arboard::Clipboard,
        config: &config::Config,
        roms_scanner: gui::ROMsScanner,
        saves_scanner: gui::SavesScanner,
        patches_scanner: gui::PatchesScanner,
        state: &mut State,
    ) {
        let roms = roms_scanner.read();
        let saves = saves_scanner.read();
        let patches = patches_scanner.read();

        let mut connection_task = state.connection_task.blocking_lock();
        let mut selection = selection.lock();

        let initial = selection.as_ref().map(|selection| {
            (
                selection.game,
                selection
                    .patch
                    .as_ref()
                    .map(|(name, version, _)| (name.clone(), version.clone())),
            )
        });

        self.save_select_window.show(
            ui.ctx(),
            &mut state.show_save_select,
            &mut *selection,
            &config.language,
            &config.saves_path(),
            roms_scanner.clone(),
            saves_scanner.clone(),
        );

        let is_ready = connection_task
            .as_ref()
            .map(|task| match task {
                ConnectionTask::InProgress { state, .. } => match state {
                    ConnectionState::InLobby(lobby) => {
                        lobby.blocking_lock().local_negotiated_state.is_some()
                    }
                    _ => false,
                },
                _ => false,
            })
            .unwrap_or(false);

        ui.add_enabled_ui(!is_ready, |ui| {
            if ui
                .horizontal(|ui| {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center).with_cross_justify(true),
                        |ui| {
                            ui.add({
                                let text = egui::RichText::new(
                                    i18n::LOCALES
                                        .lookup(&config.language, "select-save.select-button")
                                        .unwrap(),
                                )
                                .size(24.0);

                                if state.show_save_select.is_some() {
                                    egui::Button::new(
                                        text.color(ui.ctx().style().visuals.selection.stroke.color),
                                    )
                                    .fill(ui.ctx().style().visuals.selection.bg_fill)
                                } else {
                                    egui::Button::new(text)
                                }
                            }) | ui
                                .vertical_centered_justified(|ui| {
                                    let mut layouter =
                                        |ui: &egui::Ui, _: &str, _wrap_width: f32| {
                                            let mut layout_job = egui::text::LayoutJob::default();
                                            if let Some(selection) = selection.as_ref() {
                                                let (family, variant) =
                                                    selection.game.family_and_variant();
                                                layout_job.append(
                                                    &format!(
                                                        "{}",
                                                        selection
                                                            .save
                                                            .path
                                                            .strip_prefix(&config.saves_path())
                                                            .unwrap_or(
                                                                selection.save.path.as_path()
                                                            )
                                                            .display()
                                                    ),
                                                    0.0,
                                                    egui::TextFormat::simple(
                                                        ui.style()
                                                            .text_styles
                                                            .get(&egui::TextStyle::Body)
                                                            .unwrap()
                                                            .clone(),
                                                        ui.visuals().text_color(),
                                                    ),
                                                );
                                                layout_job.append(
                                                    &i18n::LOCALES
                                                        .lookup(
                                                            &config.language,
                                                            &format!(
                                                                "game-{}.variant-{}",
                                                                family, variant
                                                            ),
                                                        )
                                                        .unwrap(),
                                                    5.0,
                                                    egui::TextFormat::simple(
                                                        ui.style()
                                                            .text_styles
                                                            .get(&egui::TextStyle::Small)
                                                            .unwrap()
                                                            .clone(),
                                                        ui.visuals().text_color(),
                                                    ),
                                                );
                                            } else {
                                                layout_job.append(
                                                    &i18n::LOCALES
                                                        .lookup(
                                                            &config.language,
                                                            "select-save.no-save-selected",
                                                        )
                                                        .unwrap(),
                                                    0.0,
                                                    egui::TextFormat::simple(
                                                        ui.style()
                                                            .text_styles
                                                            .get(&egui::TextStyle::Small)
                                                            .unwrap()
                                                            .clone(),
                                                        ui.visuals().text_color(),
                                                    ),
                                                );
                                            }
                                            ui.fonts().layout_job(layout_job)
                                        };
                                    ui.add(
                                        egui::TextEdit::singleline(&mut String::new())
                                            .margin(egui::Vec2::new(4.0, 4.0))
                                            .layouter(&mut layouter),
                                    )
                                })
                                .inner
                        },
                    )
                    .inner
                })
                .inner
                .clicked()
            {
                state.show_save_select = if state.show_save_select.is_none() {
                    rayon::spawn({
                        let roms_scanner = roms_scanner.clone();
                        let saves_scanner = saves_scanner.clone();
                        let roms_path = config.roms_path();
                        let saves_path = config.saves_path();
                        move || {
                            roms_scanner.rescan(move || Some(game::scan_roms(&roms_path)));
                            saves_scanner.rescan(move || Some(save::scan_saves(&saves_path)));
                        }
                    });
                    Some(gui::save_select_window::State::new(selection.as_ref().map(
                        |selection| (selection.game, Some(selection.save.path.to_path_buf())),
                    )))
                } else {
                    None
                };
            }
        });

        ui.horizontal_top(|ui| {
            let patches = patches_scanner.read();

            let mut supported_patches = std::collections::BTreeMap::new();
            {
                let selection = if let Some(selection) = selection.as_mut() {
                    selection
                } else {
                    return;
                };

                for (name, info) in patches.iter() {
                    let mut supported_versions = info
                        .versions
                        .iter()
                        .filter(|(_, v)| v.supported_games.contains(&selection.game))
                        .map(|(v, _)| v)
                        .collect::<Vec<_>>();
                    supported_versions.sort();
                    supported_versions.reverse();

                    if supported_versions.is_empty() {
                        continue;
                    }

                    supported_patches.insert(name, (info, supported_versions));
                }
            }

            const PATCH_VERSION_COMBOBOX_WIDTH: f32 = 100.0;
            ui.add_enabled_ui(!is_ready && selection.is_some(), |ui| {
                egui::ComboBox::from_id_source("patch-select-combobox")
                    .selected_text(
                        selection
                            .as_ref()
                            .and_then(|s| s.patch.as_ref().map(|(name, _, _)| name.as_str()))
                            .unwrap_or(
                                &i18n::LOCALES
                                    .lookup(&config.language, "main.no-patch")
                                    .unwrap(),
                            ),
                    )
                    .width(
                        ui.available_width()
                            - ui.spacing().item_spacing.x
                            - PATCH_VERSION_COMBOBOX_WIDTH,
                    )
                    .show_ui(ui, |ui| {
                        let selection = if let Some(selection) = selection.as_mut() {
                            selection
                        } else {
                            return;
                        };
                        if ui
                            .selectable_label(
                                selection.patch.is_none(),
                                &i18n::LOCALES
                                    .lookup(&config.language, "main.no-patch")
                                    .unwrap(),
                            )
                            .clicked()
                        {
                            *selection = gui::Selection::new(
                                selection.game.clone(),
                                selection.save.clone(),
                                None,
                                roms.get(&selection.game).unwrap().clone(),
                            );
                        }

                        for (name, (_, supported_versions)) in supported_patches.iter() {
                            if ui
                                .selectable_label(
                                    selection.patch.as_ref().map(|(name, _, _)| name)
                                        == Some(*name),
                                    *name,
                                )
                                .clicked()
                            {
                                let rom = roms.get(&selection.game).unwrap().clone();
                                let (rom_code, revision) = selection.game.rom_code_and_revision();
                                let version = *supported_versions.first().unwrap();

                                let version_metadata = if let Some(version_metadata) = patches
                                    .get(*name)
                                    .and_then(|p| p.versions.get(version))
                                    .cloned()
                                {
                                    version_metadata
                                } else {
                                    return;
                                };

                                let bps = match std::fs::read(
                                    config
                                        .patches_path()
                                        .join(name)
                                        .join(format!("v{}", version))
                                        .join(format!(
                                            "{}_{:02}.bps",
                                            std::str::from_utf8(rom_code).unwrap(),
                                            revision
                                        )),
                                ) {
                                    Ok(bps) => bps,
                                    Err(e) => {
                                        log::error!(
                                            "failed to load patch {} to {:?}: {:?}",
                                            name,
                                            (rom_code, revision),
                                            e
                                        );
                                        return;
                                    }
                                };

                                let rom = match patch::bps::apply(&rom, &bps) {
                                    Ok(r) => r.to_vec(),
                                    Err(e) => {
                                        log::error!(
                                            "failed to apply patch {} to {:?}: {:?}",
                                            name,
                                            (rom_code, revision),
                                            e
                                        );
                                        return;
                                    }
                                };

                                *selection = gui::Selection::new(
                                    selection.game.clone(),
                                    selection.save.clone(),
                                    Some(((*name).clone(), version.clone(), version_metadata)),
                                    rom,
                                );
                            }
                        }
                    });
                ui.add_enabled_ui(
                    !is_ready
                        && selection
                            .as_ref()
                            .and_then(|selection| selection.patch.as_ref())
                            .and_then(|patch| supported_patches.get(&patch.0))
                            .map(|(_, vs)| !vs.is_empty())
                            .unwrap_or(false),
                    |ui| {
                        egui::ComboBox::from_id_source("patch-version-select-combobox")
                            .width(PATCH_VERSION_COMBOBOX_WIDTH - ui.spacing().item_spacing.x * 2.0)
                            .selected_text(
                                selection
                                    .as_ref()
                                    .and_then(|s| {
                                        s.patch.as_ref().map(|(_, version, _)| version.to_string())
                                    })
                                    .unwrap_or("".to_string()),
                            )
                            .show_ui(ui, |ui| {
                                let selection = if let Some(selection) = selection.as_mut() {
                                    selection
                                } else {
                                    return;
                                };

                                let patch = if let Some(patch) = selection.patch.as_ref() {
                                    patch.clone()
                                } else {
                                    return;
                                };

                                let supported_versions = if let Some(supported_versions) =
                                    supported_patches.get(&patch.0).map(|(_, vs)| vs)
                                {
                                    supported_versions
                                } else {
                                    return;
                                };

                                for version in supported_versions.iter() {
                                    if ui
                                        .selectable_label(&patch.1 == *version, version.to_string())
                                        .clicked()
                                    {
                                        let rom = roms.get(&selection.game).unwrap().clone();
                                        let (rom_code, revision) =
                                            selection.game.rom_code_and_revision();

                                        let version_metadata = if let Some(version_metadata) =
                                            patches
                                                .get(&patch.0)
                                                .and_then(|p| p.versions.get(version))
                                                .cloned()
                                        {
                                            version_metadata
                                        } else {
                                            return;
                                        };

                                        let bps = match std::fs::read(
                                            config
                                                .patches_path()
                                                .join(&patch.0)
                                                .join(format!("v{}", version))
                                                .join(format!(
                                                    "{}_{:02}.bps",
                                                    std::str::from_utf8(rom_code).unwrap(),
                                                    revision
                                                )),
                                        ) {
                                            Ok(bps) => bps,
                                            Err(e) => {
                                                log::error!(
                                                    "failed to load patch {} to {:?}: {:?}",
                                                    patch.0,
                                                    (rom_code, revision),
                                                    e
                                                );
                                                return;
                                            }
                                        };

                                        let rom = match patch::bps::apply(&rom, &bps) {
                                            Ok(r) => r.to_vec(),
                                            Err(e) => {
                                                log::error!(
                                                    "failed to apply patch {} to {:?}: {:?}",
                                                    patch.0,
                                                    (rom_code, revision),
                                                    e
                                                );
                                                return;
                                            }
                                        };

                                        *selection = gui::Selection::new(
                                            selection.game.clone(),
                                            selection.save.clone(),
                                            Some((
                                                patch.0.clone(),
                                                (*version).clone(),
                                                version_metadata,
                                            )),
                                            rom,
                                        );
                                    }
                                }
                            });
                    },
                );
            });
        });

        if let Some(ConnectionTask::InProgress {
            state: ConnectionState::InLobby(lobby),
            ..
        }) = connection_task.as_ref()
        {
            if initial
                != selection.as_ref().map(|selection| {
                    (
                        selection.game,
                        selection
                            .patch
                            .as_ref()
                            .map(|(name, version, _)| (name.clone(), version.clone())),
                    )
                })
            {
                // Handle changes in a different thread.
                handle.block_on(async {
                    let mut lobby = lobby.lock().await;
                    lobby.match_type = (
                        if selection
                            .as_ref()
                            .map(|selection| {
                                (lobby.match_type.0 as usize) < selection.game.match_types().len()
                            })
                            .unwrap_or(false)
                        {
                            lobby.match_type.0
                        } else {
                            0
                        },
                        0,
                    );
                    lobby.selection = if let Some(selection) = selection.as_ref() {
                        Some(LobbySelection {
                            game: selection.game,
                            save: selection.save.save.clone(),
                            rom: selection.rom.clone(),
                            patch: selection.patch.clone(),
                        })
                    } else {
                        None
                    };

                    let settings = lobby.make_local_settings();
                    let _ = lobby.send_settings(settings.clone()).await;
                    if !are_settings_compatible(&settings, &lobby.remote_settings, &roms, &patches)
                    {
                        lobby.remote_commitment = None;
                    }
                });
            }
        }

        if let Some(selection) = selection.as_mut() {
            if let Some(assets) = selection.assets.as_ref() {
                let game_language = selection.game.language();
                self.save_view.show(
                    ui,
                    clipboard,
                    font_families,
                    &config.language,
                    if let Some((_, _, metadata)) = selection.patch.as_ref() {
                        if let Some(language) = metadata.saveedit_overrides.language.as_ref() {
                            language
                        } else {
                            &game_language
                        }
                    } else {
                        &game_language
                    },
                    &selection.save.save,
                    assets,
                    &mut selection.save_view_state,
                );
            }
        }
    }
}