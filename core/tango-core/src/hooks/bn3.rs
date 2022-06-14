mod munger;
mod offsets;

use byteorder::ByteOrder;

use crate::{battle, facade, fastforwarder, hooks, input, shadow};

#[derive(Clone)]
pub struct BN3 {
    offsets: offsets::Offsets,
    munger: munger::Munger,
}

lazy_static! {
    pub static ref MEGA_EXE3_BLA3XE: Box<dyn hooks::Hooks + Send + Sync> =
        BN3::new(offsets::MEGA_EXE3_BLA3XE);
    pub static ref MEGA_EXE3_WHA6BE: Box<dyn hooks::Hooks + Send + Sync> =
        BN3::new(offsets::MEGA_EXE3_WHA6BE);
    pub static ref ROCK_EXE3_BKA3XJ_01: Box<dyn hooks::Hooks + Send + Sync> =
        BN3::new(offsets::ROCK_EXE3_BKA3XJ_01);
    pub static ref ROCKMAN_EXE3A6BJ_01: Box<dyn hooks::Hooks + Send + Sync> =
        BN3::new(offsets::ROCKMAN_EXE3A6BJ_01);
}

impl BN3 {
    pub fn new(offsets: offsets::Offsets) -> Box<dyn hooks::Hooks + Send + Sync> {
        Box::new(BN3 {
            offsets,
            munger: munger::Munger { offsets },
        })
    }
}

fn random_background(rng: &mut impl rand::Rng) -> u8 {
    const BATTLE_BACKGROUNDS: &[u8] = &[0x00, 0x04, 0x05, 0x06, 0x17, 0x10, 0x02, 0x0a];
    BATTLE_BACKGROUNDS[rng.gen_range(0..BATTLE_BACKGROUNDS.len())]
}

fn step_rng(seed: u32) -> u32 {
    let seed = std::num::Wrapping(seed);
    (((seed * std::num::Wrapping(2)) - (seed >> 0x1f) + std::num::Wrapping(1))
        ^ std::num::Wrapping(0x873ca9e5))
    .0
}

fn generate_rng1_state(rng: &mut impl rand::Rng) -> u32 {
    let mut rng1_state = 0;
    for _ in 0..rng.gen_range(0..=0xffffusize) {
        rng1_state = step_rng(rng1_state);
    }
    rng1_state
}

fn generate_rng2_state(rng: &mut impl rand::Rng) -> u32 {
    let mut rng2_state = 0xa338244f;
    for _ in 0..rng.gen_range(0..=0xffffusize) {
        rng2_state = step_rng(rng2_state);
    }
    rng2_state
}

const INIT_RX: [u8; 16] = [
    0x01, 0x00, 0x00, 0xff, 0x00, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

impl hooks::Hooks for BN3 {
    fn common_traps(&self) -> Vec<(u32, Box<dyn FnMut(mgba::core::CoreMutRef)>)> {
        vec![
            {
                let munger = self.munger.clone();
                (
                    self.offsets.rom.start_screen_jump_table_entry,
                    Box::new(move |core| {
                        munger.skip_logo(core);
                    }),
                )
            },
            {
                let munger = self.munger.clone();
                (
                    self.offsets.rom.start_screen_sram_unmask_ret,
                    Box::new(move |core| {
                        munger.continue_from_title_menu(core);
                    }),
                )
            },
            {
                let munger = self.munger.clone();
                (
                    self.offsets.rom.game_load_ret,
                    Box::new(move |core| {
                        munger.open_comm_menu_from_overworld(core);
                    }),
                )
            },
        ]
    }

    fn primary_traps(
        &self,
        handle: tokio::runtime::Handle,
        joyflags: std::sync::Arc<std::sync::atomic::AtomicU32>,
        facade: facade::Facade,
    ) -> Vec<(u32, Box<dyn FnMut(mgba::core::CoreMutRef)>)> {
        let make_send_and_receive_call_hook = || {
            let facade = facade.clone();
            let munger = self.munger.clone();
            let handle = handle.clone();
            Box::new(move |mut core: mgba::core::CoreMutRef| {
                handle.block_on(async {
                    let pc = core.as_ref().gba().cpu().thumb_pc();
                    core.gba_mut().cpu_mut().set_thumb_pc(pc + 4);
                    core.gba_mut().cpu_mut().set_gpr(0, 3);

                    let match_ = match facade.match_().await {
                        Some(match_) => match_,
                        None => {
                            return;
                        }
                    };

                    let mut round_state = match_.lock_round_state().await;

                    let round = match round_state.round.as_mut() {
                        Some(round) => round,
                        None => {
                            return;
                        }
                    };

                    round.queue_tx(round.current_tick() + 1, munger.tx_packet(core).to_vec());

                    let ip = round.peek_last_input().as_ref().unwrap();

                    munger.set_rx_packet(
                        core,
                        round.local_player_index() as u32,
                        &ip.local.rx.clone().try_into().unwrap(),
                    );

                    munger.set_rx_packet(
                        core,
                        round.remote_player_index() as u32,
                        &ip.remote.rx.clone().try_into().unwrap(),
                    );
                });
            })
        };

        let make_round_end_hook = || {
            let facade = facade.clone();
            let handle = handle.clone();
            Box::new(move |_: mgba::core::CoreMutRef| {
                handle.block_on(async {
                    let match_ = match facade.match_().await {
                        Some(match_) => match_,
                        None => {
                            return;
                        }
                    };

                    let mut round_state = match_.lock_round_state().await;
                    round_state.end_round().await.expect("end round");
                    match_
                        .advance_shadow_until_round_end()
                        .await
                        .expect("advance shadow");
                });
            })
        };

        vec![
            {
                let facade = facade.clone();
                let handle = handle.clone();
                let munger = self.munger.clone();
                (
                    self.offsets.rom.comm_menu_init_ret,
                    Box::new(move |core| {
                        handle.block_on(async {
                            let match_ = match facade.match_().await {
                                Some(match_) => match_,
                                None => {
                                    return;
                                }
                            };

                            let mut rng = match_.lock_rng().await;

                            // rng1 is the local rng, it should not be synced.
                            // However, we should make sure it's reproducible from the shared RNG state so we generate it like this.
                            let offerer_rng1_state = generate_rng1_state(&mut *rng);
                            let answerer_rng1_state = generate_rng1_state(&mut *rng);
                            munger.set_rng1_state(
                                core,
                                if match_.is_offerer() {
                                    offerer_rng1_state
                                } else {
                                    answerer_rng1_state
                                },
                            );

                            // rng2 is the shared rng, it must be synced.
                            munger.set_rng2_state(core, generate_rng2_state(&mut *rng));

                            munger.start_battle_from_comm_menu(core, match_.match_type());
                        });
                    }),
                )
            },
            {
                let facade = facade.clone();
                let handle = handle.clone();
                (
                    self.offsets.rom.match_end_ret,
                    Box::new(move |_core| {
                        handle.block_on(async {
                            log::info!("match ended");
                            facade.end_match().await;
                        });
                    }),
                )
            },
            {
                let facade = facade.clone();
                let handle = handle.clone();
                (
                    self.offsets.rom.round_end_cmp,
                    Box::new(move |core| {
                        handle.block_on(async {
                            let match_ = match facade.match_().await {
                                Some(match_) => match_,
                                None => {
                                    return;
                                }
                            };

                            let mut round_state = match_.lock_round_state().await;

                            match core.as_ref().gba().cpu().gpr(0) {
                                1 => {
                                    round_state.set_last_result(battle::BattleResult::Win);
                                }
                                2 => {
                                    round_state.set_last_result(battle::BattleResult::Loss);
                                }
                                5 => {
                                    round_state.set_last_result(battle::BattleResult::Draw);
                                }
                                _ => {}
                            }
                        });
                    }),
                )
            },
            (self.offsets.rom.round_win_ret, make_round_end_hook()),
            (self.offsets.rom.round_win_ret2, make_round_end_hook()),
            (self.offsets.rom.round_lose_ret, make_round_end_hook()),
            (self.offsets.rom.round_lose_ret2, make_round_end_hook()),
            (self.offsets.rom.round_tie_ret, make_round_end_hook()),
            {
                let facade = facade.clone();
                let handle = handle.clone();
                (
                    self.offsets.rom.round_start_ret,
                    Box::new(move |_core| {
                        handle.block_on(async {
                            let match_ = match facade.match_().await {
                                Some(match_) => match_,
                                None => {
                                    return;
                                }
                            };
                            match_.start_round().await.expect("start round");
                        });
                    }),
                )
            },
            {
                let facade = facade.clone();
                let handle = handle.clone();
                (
                    self.offsets.rom.battle_is_p2_ret,
                    Box::new(move |mut core| {
                        handle.block_on(async {
                            let match_ = match facade.match_().await {
                                Some(match_) => match_,
                                None => {
                                    return;
                                }
                            };

                            let round_state = match_.lock_round_state().await;
                            let round = round_state.round.as_ref().expect("round");

                            core.gba_mut()
                                .cpu_mut()
                                .set_gpr(0, round.local_player_index() as i32);
                        });
                    }),
                )
            },
            {
                let facade = facade.clone();
                let handle = handle.clone();
                (
                    self.offsets.rom.link_is_p2_ret,
                    Box::new(move |mut core| {
                        handle.block_on(async {
                            let match_ = match facade.match_().await {
                                Some(match_) => match_,
                                None => {
                                    return;
                                }
                            };

                            let round_state = match_.lock_round_state().await;
                            let round = match round_state.round.as_ref() {
                                Some(round) => round,
                                None => {
                                    return;
                                }
                            };

                            core.gba_mut()
                                .cpu_mut()
                                .set_gpr(0, round.local_player_index() as i32);
                        });
                    }),
                )
            },
            {
                let facade = facade.clone();
                let munger = self.munger.clone();
                let handle = handle.clone();
                (
                    self.offsets.rom.main_read_joyflags,
                    Box::new(move |core| {
                        handle.block_on(async {
                            'abort: loop {
                                let match_ = match facade.match_().await {
                                    Some(match_) => match_,
                                    None => {
                                        return;
                                    }
                                };

                                let mut round_state = match_.lock_round_state().await;

                                let round = match round_state.round.as_mut() {
                                    Some(round) => round,
                                    None => {
                                        return;
                                    }
                                };

                                if !munger.is_linking(core) {
                                    return;
                                }

                                if !round.has_committed_state() {
                                    round.set_first_committed_state(
                                        core.save_state().expect("save state"),
                                        match_
                                            .advance_shadow_until_first_committed_state()
                                            .await
                                            .expect("shadow save state"),
                                    );
                                    log::info!(
                                        "primary rng1 state: {:08x}",
                                        munger.rng1_state(core)
                                    );
                                    log::info!(
                                        "primary rng2 state: {:08x}",
                                        munger.rng2_state(core)
                                    );
                                    log::info!(
                                        "battle state committed on {}",
                                        round.current_tick()
                                    );
                                }

                                if !round
                                    .add_local_input_and_fastforward(
                                        core,
                                        joyflags.load(std::sync::atomic::Ordering::Relaxed) as u16,
                                    )
                                    .await
                                {
                                    break 'abort;
                                }
                                return;
                            }
                            facade.abort_match().await;
                        });
                    }),
                )
            },
            (
                self.offsets.rom.handle_input_init_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.handle_input_update_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.handle_input_deinit_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.process_battle_input_ret,
                Box::new(move |mut core| {
                    core.gba_mut().cpu_mut().set_gpr(0, 0);
                }),
            ),
            {
                let facade = facade.clone();
                let munger = self.munger.clone();
                let handle = handle.clone();
                (
                    self.offsets.rom.comm_menu_send_and_receive_call,
                    Box::new(move |mut core| {
                        handle.block_on(async {
                            let match_ = match facade.match_().await {
                                Some(match_) => match_,
                                None => {
                                    return;
                                }
                            };

                            let pc = core.as_ref().gba().cpu().thumb_pc();
                            core.gba_mut().cpu_mut().set_thumb_pc(pc + 4);
                            core.gba_mut().cpu_mut().set_gpr(0, 3);
                            let mut rng = match_.lock_rng().await;
                            let mut rx = INIT_RX.clone();
                            rx[4] = random_background(&mut *rng);
                            munger.set_rx_packet(core, 0, &rx);
                            munger.set_rx_packet(core, 1, &rx);
                        });
                    }),
                )
            },
            {
                (
                    self.offsets.rom.init_sio_call,
                    Box::new(move |mut core| {
                        let pc = core.as_ref().gba().cpu().thumb_pc();
                        core.gba_mut().cpu_mut().set_thumb_pc(pc + 4);
                    }),
                )
            },
            {
                let facade = facade.clone();
                (
                    self.offsets.rom.handle_input_post_call,
                    Box::new(move |_| {
                        handle.block_on(async {
                            let match_ = match facade.match_().await {
                                Some(match_) => match_,
                                None => {
                                    return;
                                }
                            };

                            let mut round_state = match_.lock_round_state().await;

                            let round = match round_state.round.as_mut() {
                                Some(round) => round,
                                None => {
                                    return;
                                }
                            };

                            if !round.has_committed_state() {
                                return;
                            }

                            round.increment_current_tick();
                        });
                    }),
                )
            },
        ]
    }

    fn shadow_traps(
        &self,
        shadow_state: shadow::State,
    ) -> Vec<(u32, Box<dyn FnMut(mgba::core::CoreMutRef)>)> {
        let make_send_and_receive_call_hook = || {
            let shadow_state = shadow_state.clone();
            let munger = self.munger.clone();

            Box::new(move |mut core: mgba::core::CoreMutRef| {
                let pc = core.as_ref().gba().cpu().thumb_pc();
                core.gba_mut().cpu_mut().set_thumb_pc(pc + 4);
                core.gba_mut().cpu_mut().set_gpr(0, 3);

                let mut round_state = shadow_state.lock_round_state();
                let round = match round_state.round.as_mut() {
                    Some(round) => round,
                    None => {
                        return;
                    }
                };

                let ip = if let Some(ip) = round.peek_out_input_pair().as_ref() {
                    ip
                } else {
                    return;
                };

                // HACK: This is required if the emulator advances beyond read joyflags and runs this function again, but is missing input data.
                // We permit this for one tick only, but really we should just not be able to get into this situation in the first place.
                if ip.local.local_tick + 1 == round.current_tick() {
                    return;
                }

                if ip.local.local_tick != ip.remote.local_tick {
                    shadow_state.set_anyhow_error(anyhow::anyhow!(
                    "copy input data: local tick != remote tick (in battle tick = {}): {} != {}",
                    round.current_tick(),
                    ip.local.local_tick,
                    ip.remote.local_tick
                ));
                    return;
                }

                if ip.local.local_tick != round.current_tick() {
                    shadow_state.set_anyhow_error(anyhow::anyhow!(
                        "copy input data: input tick != in battle tick: {} != {}",
                        ip.local.local_tick,
                        round.current_tick(),
                    ));
                    return;
                }

                munger.set_rx_packet(
                    core,
                    round.local_player_index() as u32,
                    &ip.local.rx.clone().try_into().unwrap(),
                );

                munger.set_rx_packet(
                    core,
                    round.remote_player_index() as u32,
                    &ip.remote.rx.clone().try_into().unwrap(),
                );

                round.set_input_injected();
            })
        };

        vec![
            {
                let munger = self.munger.clone();
                let shadow_state = shadow_state.clone();
                (
                    self.offsets.rom.comm_menu_init_ret,
                    Box::new(move |core| {
                        let mut rng = shadow_state.lock_rng();

                        // rng1 is the local rng, it should not be synced.
                        // However, we should make sure it's reproducible from the shared RNG state so we generate it like this.
                        let offerer_rng1_state = generate_rng1_state(&mut *rng);
                        let answerer_rng1_state = generate_rng1_state(&mut *rng);
                        munger.set_rng1_state(
                            core,
                            if shadow_state.is_offerer() {
                                answerer_rng1_state
                            } else {
                                offerer_rng1_state
                            },
                        );

                        // rng2 is the shared rng, it must be synced.
                        munger.set_rng2_state(core, generate_rng2_state(&mut *rng));

                        munger.start_battle_from_comm_menu(core, shadow_state.match_type());
                    }),
                )
            },
            {
                let shadow_state = shadow_state.clone();
                (
                    self.offsets.rom.round_start_ret,
                    Box::new(move |_| {
                        shadow_state.start_round();
                    }),
                )
            },
            {
                let shadow_state = shadow_state.clone();
                (
                    self.offsets.rom.round_end_cmp,
                    Box::new(move |core| {
                        match core.as_ref().gba().cpu().gpr(0) {
                            1 => {
                                shadow_state.set_last_result(battle::BattleResult::Loss);
                            }
                            2 => {
                                shadow_state.set_last_result(battle::BattleResult::Win);
                            }
                            5 => {
                                shadow_state.set_last_result(battle::BattleResult::Draw);
                            }
                            _ => return,
                        };
                    }),
                )
            },
            {
                let shadow_state = shadow_state.clone();
                (
                    self.offsets.rom.round_end_entry,
                    Box::new(move |core| {
                        shadow_state.end_round();
                        shadow_state.set_applied_state(core.save_state().expect("save state"), 0);
                    }),
                )
            },
            {
                let shadow_state = shadow_state.clone();
                (
                    self.offsets.rom.battle_is_p2_ret,
                    Box::new(move |mut core| {
                        let round_state = shadow_state.lock_round_state();
                        let round = round_state.round.as_ref().expect("round");

                        core.gba_mut()
                            .cpu_mut()
                            .set_gpr(0, round.remote_player_index() as i32);
                    }),
                )
            },
            {
                let shadow_state = shadow_state.clone();
                (
                    self.offsets.rom.link_is_p2_ret,
                    Box::new(move |mut core| {
                        let round_state = shadow_state.lock_round_state();
                        let round = match round_state.round.as_ref() {
                            Some(round) => round,
                            None => {
                                return;
                            }
                        };

                        core.gba_mut()
                            .cpu_mut()
                            .set_gpr(0, round.remote_player_index() as i32);
                    }),
                )
            },
            {
                let shadow_state = shadow_state.clone();
                let munger = self.munger.clone();
                (
                    self.offsets.rom.main_read_joyflags,
                    Box::new(move |mut core| {
                        let mut round_state = shadow_state.lock_round_state();
                        let round = match round_state.round.as_mut() {
                            Some(round) => round,
                            None => {
                                return;
                            }
                        };

                        if !munger.is_linking(core) && !round.has_first_committed_state() {
                            return;
                        }

                        if !round.has_first_committed_state() {
                            round.set_first_committed_state(core.save_state().expect("save state"));
                            log::info!("shadow rng1 state: {:08x}", munger.rng1_state(core));
                            log::info!("shadow rng2 state: {:08x}", munger.rng2_state(core));
                            log::info!("shadow state committed on {}", round.current_tick());
                            return;
                        }

                        if let Some(ip) = round.take_in_input_pair() {
                            if ip.local.local_tick != ip.remote.local_tick {
                                shadow_state.set_anyhow_error(anyhow::anyhow!(
                                    "read joyflags: local tick != remote tick (in battle tick = {}): {} != {}",
                                    round.current_tick(),
                                    ip.local.local_tick,
                                    ip.remote.local_tick
                                ));
                                return;
                            }

                            if ip.local.local_tick != round.current_tick() {
                                shadow_state.set_anyhow_error(anyhow::anyhow!(
                                    "read joyflags: input tick != in battle tick: {} != {}",
                                    ip.local.local_tick,
                                    round.current_tick(),
                                ));
                                return;
                            }

                            round.set_out_input_pair(input::Pair {
                                local: ip.local,
                                remote: input::Input {
                                    local_tick: ip.remote.local_tick,
                                    remote_tick: ip.remote.remote_tick,
                                    joyflags: ip.remote.joyflags,
                                    rx: munger.tx_packet(core).to_vec(),
                                    is_prediction: false,
                                },
                            });

                            core.gba_mut()
                                .cpu_mut()
                                .set_gpr(4, (ip.remote.joyflags | 0xfc00) as i32);
                        }

                        if round.take_input_injected() {
                            shadow_state.set_applied_state(
                                core.save_state().expect("save state"),
                                round.current_tick(),
                            );
                        }
                    }),
                )
            },
            (
                self.offsets.rom.handle_input_init_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.handle_input_update_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.handle_input_deinit_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.process_battle_input_ret,
                Box::new(move |mut core| {
                    core.gba_mut().cpu_mut().set_gpr(0, 0);
                }),
            ),
            {
                let shadow_state = shadow_state.clone();
                let munger = self.munger.clone();
                (
                    self.offsets.rom.comm_menu_send_and_receive_call,
                    Box::new(move |mut core| {
                        let pc = core.as_ref().gba().cpu().thumb_pc();
                        core.gba_mut().cpu_mut().set_thumb_pc(pc + 4);
                        core.gba_mut().cpu_mut().set_gpr(0, 3);
                        let mut rng = shadow_state.lock_rng();
                        let mut rx = INIT_RX.clone();
                        rx[4] = random_background(&mut *rng);
                        munger.set_rx_packet(core, 0, &rx);
                        munger.set_rx_packet(core, 1, &rx);
                    }),
                )
            },
            {
                (
                    self.offsets.rom.init_sio_call,
                    Box::new(move |mut core| {
                        let pc = core.as_ref().gba().cpu().thumb_pc();
                        core.gba_mut().cpu_mut().set_thumb_pc(pc + 4);
                    }),
                )
            },
            {
                let shadow_state = shadow_state.clone();
                (
                    self.offsets.rom.handle_input_post_call,
                    Box::new(move |mut core| {
                        let mut round_state = shadow_state.lock_round_state();
                        let round = match round_state.round.as_mut() {
                            Some(round) => round,
                            None => {
                                return;
                            }
                        };

                        if !round.has_first_committed_state() {
                            return;
                        }
                        round.increment_current_tick();

                        if round_state.last_result.is_some() {
                            // We have no real inputs left but the round has ended. Just fudge them until we get to the next round.
                            core.gba_mut().cpu_mut().set_gpr(0, 7);
                        }
                    }),
                )
            },
        ]
    }

    fn fastforwarder_traps(
        &self,
        ff_state: fastforwarder::State,
    ) -> Vec<(u32, Box<dyn FnMut(mgba::core::CoreMutRef)>)> {
        let make_send_and_receive_call_hook = || {
            let munger = self.munger.clone();
            let ff_state = ff_state.clone();
            Box::new(move |mut core: mgba::core::CoreMutRef| {
                let pc = core.as_ref().gba().cpu().thumb_pc();
                core.gba_mut().cpu_mut().set_thumb_pc(pc + 4);
                core.gba_mut().cpu_mut().set_gpr(0, 3);

                let current_tick = ff_state.current_tick();

                let ip = match ff_state.pop_input_pair() {
                    Some(ip) => ip,
                    None => {
                        return;
                    }
                };

                if ip.local.local_tick != ip.remote.local_tick {
                    ff_state.set_anyhow_error(anyhow::anyhow!(
                            "copy input data: local tick != remote tick (in battle tick = {}): {} != {}",
                            current_tick,
                            ip.local.local_tick,
                            ip.remote.local_tick
                        ));
                    return;
                }

                if ip.local.local_tick != current_tick {
                    ff_state.set_anyhow_error(anyhow::anyhow!(
                        "copy input data: input tick != in battle tick: {} != {}",
                        ip.local.local_tick,
                        current_tick,
                    ));
                    return;
                }

                munger.set_rx_packet(
                    core,
                    ff_state.local_player_index() as u32,
                    &ip.local.rx.try_into().unwrap(),
                );

                munger.set_rx_packet(
                    core,
                    ff_state.remote_player_index() as u32,
                    &ip.remote.rx.try_into().unwrap(),
                );
            })
        };

        vec![
            {
                let ff_state = ff_state.clone();
                (
                    self.offsets.rom.battle_is_p2_ret,
                    Box::new(move |mut core| {
                        core.gba_mut()
                            .cpu_mut()
                            .set_gpr(0, ff_state.local_player_index() as i32);
                    }),
                )
            },
            {
                let ff_state = ff_state.clone();
                (
                    self.offsets.rom.link_is_p2_ret,
                    Box::new(move |mut core| {
                        core.gba_mut()
                            .cpu_mut()
                            .set_gpr(0, ff_state.local_player_index() as i32);
                    }),
                )
            },
            {
                let ff_state = ff_state.clone();
                (
                    self.offsets.rom.round_end_entry,
                    Box::new(move |_core| {
                        ff_state.on_battle_ended();
                    }),
                )
            },
            {
                let ff_state = ff_state.clone();
                (
                    self.offsets.rom.main_read_joyflags,
                    Box::new(move |mut core| {
                        let current_tick = ff_state.current_tick();

                        if current_tick == ff_state.commit_time() {
                            ff_state.set_committed_state(
                                core.save_state().expect("save committed state"),
                            );
                        }

                        let ip = match ff_state.peek_input_pair() {
                            Some(ip) => ip,
                            None => {
                                ff_state.on_inputs_exhausted();
                                return;
                            }
                        };

                        if ip.local.local_tick != ip.remote.local_tick {
                            ff_state.set_anyhow_error(anyhow::anyhow!(
                                "read joyflags: local tick != remote tick (in battle tick = {}): {} != {}",
                                current_tick,
                                ip.local.local_tick,
                                ip.remote.local_tick
                            ));
                            return;
                        }

                        if ip.local.local_tick != current_tick {
                            ff_state.set_anyhow_error(anyhow::anyhow!(
                                "read joyflags: input tick != in battle tick: {} != {}",
                                ip.local.local_tick,
                                current_tick,
                            ));
                            return;
                        }

                        core.gba_mut()
                            .cpu_mut()
                            .set_gpr(4, (ip.local.joyflags | 0xfc00) as i32);

                        if current_tick == ff_state.dirty_time() {
                            ff_state.set_dirty_state(core.save_state().expect("save dirty state"));
                        }
                    }),
                )
            },
            (
                self.offsets.rom.handle_input_init_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.handle_input_update_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.handle_input_deinit_send_and_receive_call,
                make_send_and_receive_call_hook(),
            ),
            (
                self.offsets.rom.process_battle_input_ret,
                Box::new(move |mut core| {
                    core.gba_mut().cpu_mut().set_gpr(0, 0);
                }),
            ),
            {
                let ff_state = ff_state.clone();
                (
                    self.offsets.rom.handle_input_post_call,
                    Box::new(move |_| {
                        ff_state.increment_current_tick();
                    }),
                )
            },
        ]
    }

    fn placeholder_rx(&self) -> Vec<u8> {
        vec![
            0x01, 0xff, 0x00, 0xff, 0x06, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff,
        ]
    }

    fn predict_rx(&self, rx: &mut Vec<u8>) {
        let tick = byteorder::LittleEndian::read_u16(&rx[0x4..0x6]);
        byteorder::LittleEndian::write_u16(&mut rx[0x4..0x6], tick.wrapping_add(1));
    }

    fn prepare_for_fastforward(&self, mut core: mgba::core::CoreMutRef) {
        core.gba_mut()
            .cpu_mut()
            .set_thumb_pc(self.offsets.rom.main_read_joyflags);
    }
}
