//! Devil Within mod menu (branch `devil/debug` only, opt-in with T5_DW_DEBUG): an in-game menu
//! that changes guest data and calls guest functions. Not part of the 1:1 recompilation.
//!
//! SELECT held + L1 opens / closes it; with SELECT held: Up / Down entry, Left / Right value,
//! CIRCLE run / toggle. Structures (DevilWithin.md, "Mod-Menü"):
//!
//! - Characters ("man", 0x148400): tasks of type 1000 (+0x3C) in the task pool
//!   0x2A9A30. +0x1C name, +0x140 input record index, +0x144 character data index, +0x148 AI,
//!   +0x154 bit 2 = AI controlled (set at creation), +0x162 pad port.
//! - Input records (0x15C6A0): pool *0x42B6FC, stride [0x42B6F0], count [0x42B6EA] (short);
//!   +0 (short) controller: 0 = pad 1 (0x15C4E0(0)), < 0 = the AI's pad buffer [0x41BED0],
//!   other = no input.
//! - Player ("ManMgr", 0x146FB0): manager [0x41BB10], current entry index at +0x140, entries
//!   *0x42B66C stride [0x42B660] count [0x42B65A] (short); entry +0 = the player character
//!   (0x147A98; camera, HUD and the enemies' target follow it; 0x1471F8 sets it).
//! - Character data (0x16EF88): *0x4804B4 stride [0x4804A8] count [0x4804A2] (short); life
//!   manager at +0x3004: life +0x301C, max +0x3020 (floats, 0x16FCE0), team +0x300A (short,
//!   tsEHpManGet/SetTeamID 0x14A4E0 / 0x14A460), devil gauge +0x3024, max +0x3028 (floats,
//!   tsEHpManGet/SetDGauge 0x14ACB0 / 0x14AD50 -> 0x16FDF0), position matrix +0x70
//!   (tsEHpManGetPosMtxPtr 0x14A138), yaw +0x110 (0x14A738).

use ps2_runtime::bus::{ram_read, ram_write};
use ps2_runtime::Machine;
use std::sync::Mutex;

const TASK_POOL: u32 = 0x002A_9A30;
/// Task type of a character ("man", 0x148400 passes 1000 to the task allocator 0x123288; the
/// tsEHpMan* functions check +0x3C == 1000). The update function differs (the player's is not
/// 0x1493F8), so the type identifies them.
const MAN_TYPE: i16 = 1000;
const MAN_MGR: u32 = 0x0041_BB10;
const MAN_POOL: u32 = 0x0042_B66C;
const MAN_STRIDE: u32 = 0x0042_B660;
const MAN_COUNT: u32 = 0x0042_B65A;
const INPUT_POOL: u32 = 0x0042_B6FC;
const INPUT_STRIDE: u32 = 0x0042_B6F0;
const INPUT_COUNT: u32 = 0x0042_B6EA;
const CHR_POOL: u32 = 0x0048_04B4;
const CHR_STRIDE: u32 = 0x0048_04A8;
const CHR_COUNT: u32 = 0x0048_04A2;

/// Pad 1 held buttons (0x2A9890 + 0x50).
const PAD_HELD: u32 = 0x002A_98E0;
const SELECT: u32 = 0x1;
const UP: u32 = 0x10;
const RIGHT: u32 = 0x20;
const DOWN: u32 = 0x40;
const LEFT: u32 = 0x80;
const L1: u32 = 0x400;
const CIRCLE: u32 = 0x2000;

/// matty debug tree entries that run without arguments (0x356130..; DevilWithin.md).
const HEAL_30: u32 = 0x001D_92B0;
const SET_COLLECT: u32 = 0x001D_9A78;
const CLEAR_COLLECT: u32 = 0x001D_9AF0;

fn word(m: &Machine, a: u32) -> u32 {
    ram_read(m, a, 4) as u32
}

fn half(m: &Machine, a: u32) -> i16 {
    ram_read(m, a, 2) as u16 as i16
}

fn cstr(m: &Machine, a: u32) -> String {
    (0..48).map(|i| ram_read(m, a + i, 1) as u8).take_while(|&b| b != 0).map(|b| if b.is_ascii_graphic() { b as char } else { '?' }).collect()
}

/// The living characters (tasks with the man update).
fn characters(m: &Machine) -> Vec<u32> {
    let mut out = Vec::new();
    let mut t = word(m, TASK_POOL + 8);
    for _ in 0..1024 {
        if t == 0 {
            break;
        }
        if half(m, t + 0x3C) == MAN_TYPE {
            out.push(t);
        }
        t = word(m, t + 0x18);
    }
    out
}

fn alive(m: &Machine, t: u32) -> bool {
    characters(m).contains(&t)
}

/// The ManMgr entry holding the player character (0 if none).
fn player_slot(m: &Machine) -> u32 {
    let mgr = word(m, MAN_MGR);
    if mgr == 0 || half(m, MAN_COUNT) < 1 {
        return 0;
    }
    let idx = word(m, mgr + 0x140) as i32;
    if idx < 0 {
        return 0;
    }
    word(m, MAN_POOL).wrapping_add(word(m, MAN_STRIDE).wrapping_mul(idx as u32))
}

fn input_record(m: &Machine, t: u32) -> u32 {
    let i = word(m, t + 0x140) as i32;
    if i < 0 || half(m, INPUT_COUNT) < 1 {
        return 0;
    }
    word(m, INPUT_POOL).wrapping_add(word(m, INPUT_STRIDE).wrapping_mul(i as u32))
}

fn char_data(m: &Machine, t: u32) -> u32 {
    let i = word(m, t + 0x144) as i32;
    if i < 0 || half(m, CHR_COUNT) < 1 {
        return 0;
    }
    word(m, CHR_POOL).wrapping_add(word(m, CHR_STRIDE).wrapping_mul(i as u32))
}

/// A character's display name: its task name (the man name, 0x147F98) and model slot.
fn name(m: &Machine, t: u32) -> String {
    let n = word(m, t + 0x1C);
    let s = if n != 0 { cstr(m, n) } else { String::new() };
    let s = if s.is_empty() { format!("{t:#x}") } else { s };
    let ai = if word(m, t + 0x154) & 2 != 0 { "AI" } else { "player" };
    format!("{s} (model {}, {ai})", half(m, t + 0x160))
}

#[derive(Default)]
struct State {
    open: bool,
    cur: usize,
    prev_held: u32,
    /// Index into the character list of the switch target.
    pick: usize,
    /// Active switch: (original player, its controller, target, the target's controller,
    /// the target's team before a swap).
    switched: Option<(u32, i16, u32, i16, Option<i16>)>,
    god: bool,
    devil: bool,
    /// Give a switched-to character the player's team (it fights the others).
    swap_team: bool,
    probe: bool,
    last: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

const ENTRIES: usize = 10;

/// Control `target`: pad 1 drives it, the current player gets no input, the ManMgr entry
/// (camera, HUD, enemy target) points at it.
fn switch_to(m: &mut Machine, st: &mut State, target: u32) -> String {
    restore(m, st);
    let slot = player_slot(m);
    if slot == 0 {
        return "no player in this scene".into();
    }
    let player = word(m, slot);
    if player == target {
        return "that is the current character".into();
    }
    let (rp, rt) = (input_record(m, player), input_record(m, target));
    if rp == 0 || rt == 0 {
        return "character has no input record".into();
    }
    let (cp, ct) = (half(m, rp), half(m, rt));
    ram_write(m, rt, 2, 0);
    ram_write(m, rp, 2, 1);
    ram_write(m, slot, 4, target as u64);
    let mut team = None;
    if st.swap_team {
        let (dp, dt) = (char_data(m, player), char_data(m, target));
        if dp != 0 && dt != 0 {
            team = Some(half(m, dt + 0x300A));
            ram_write(m, dt + 0x300A, 2, half(m, dp + 0x300A) as u16 as u64);
        }
    }
    st.switched = Some((player, cp, target, ct, team));
    ps2_runtime::diag::log(format!("devil mod: control {target:#x} (was {player:#x})"));
    format!("now controlling {}", name(m, target))
}

/// Undo a switch (if the original player still exists).
fn restore(m: &mut Machine, st: &mut State) {
    let Some((player, cp, target, ct, team)) = st.switched.take() else { return };
    if alive(m, target) {
        let rt = input_record(m, target);
        if rt != 0 {
            ram_write(m, rt, 2, ct as u16 as u64);
        }
        let dt = char_data(m, target);
        if let (Some(team), true) = (team, dt != 0) {
            ram_write(m, dt + 0x300A, 2, team as u16 as u64);
        }
    }
    if alive(m, player) {
        let rp = input_record(m, player);
        if rp != 0 {
            ram_write(m, rp, 2, cp as u16 as u64);
        }
        let slot = player_slot(m);
        if slot != 0 {
            ram_write(m, slot, 4, player as u64);
        }
    }
}

/// Every vertical blank while Devil Within runs: input, effects, menu text.
pub fn update(m: &mut Machine, out: &mut String) {
    let mut g = STATE.lock().unwrap();
    let st = g.get_or_insert_with(|| State { swap_team: true, ..State::default() });
    // A switched-to character that died or was removed: give control back.
    if let Some((_, _, target, _, _)) = st.switched {
        if !alive(m, target) {
            restore(m, st);
            st.last = "controlled character is gone, back to the original".into();
        }
    }
    // Unlimited health / devil gauge for the character in control.
    let slot = player_slot(m);
    let p = if slot != 0 { word(m, slot) } else { 0 };
    let cd = if p != 0 { char_data(m, p) } else { 0 };
    if cd != 0 {
        if st.god && f32::from_bits(word(m, cd + 0x301C)) > 0.0 {
            let max = word(m, cd + 0x3020);
            ram_write(m, cd + 0x301C, 4, max as u64);
        }
        if st.devil {
            let max = word(m, cd + 0x3028);
            ram_write(m, cd + 0x3024, 4, max as u64);
        }
    }
    let held = word(m, PAD_HELD);
    let new = held & !st.prev_held;
    st.prev_held = held;
    if held & SELECT != 0 && new & L1 != 0 {
        st.open = !st.open;
    }
    if !st.open {
        return;
    }
    let chars = characters(m);
    if st.pick >= chars.len() {
        st.pick = 0;
    }
    if held & SELECT != 0 {
        if new & UP != 0 {
            st.cur = (st.cur + ENTRIES - 1) % ENTRIES;
        }
        if new & DOWN != 0 {
            st.cur = (st.cur + 1) % ENTRIES;
        }
        if st.cur == 0 && !chars.is_empty() {
            if new & RIGHT != 0 {
                st.pick = (st.pick + 1) % chars.len();
            }
            if new & LEFT != 0 {
                st.pick = (st.pick + chars.len() - 1) % chars.len();
            }
        }
        if new & CIRCLE != 0 {
            st.last = match st.cur {
                0 => match chars.get(st.pick) {
                    Some(&t) => switch_to(m, st, t),
                    None => "no characters in this scene".into(),
                },
                1 => {
                    if st.switched.is_some() {
                        restore(m, st);
                        "back to the original character".into()
                    } else {
                        "not switched".into()
                    }
                }
                2 => {
                    st.god = !st.god;
                    format!("unlimited health {}", if st.god { "on" } else { "off" })
                }
                6 => {
                    st.devil = !st.devil;
                    format!("unlimited devil gauge {}", if st.devil { "on" } else { "off" })
                }
                7 => {
                    st.swap_team = !st.swap_team;
                    format!("team swap when switching {}", if st.swap_team { "on" } else { "off" })
                }
                8 => {
                    st.probe = !st.probe;
                    format!("position probe {}", if st.probe { "on" } else { "off" })
                }
                3 => {
                    ps2_runtime::kernel::request_guest_call(HEAL_30);
                    "restored 30% health (game function 0x1D92B0)".into()
                }
                4 => {
                    ps2_runtime::kernel::request_guest_call(SET_COLLECT);
                    "all COLLECT flags set (saved with the game!)".into()
                }
                5 => {
                    ps2_runtime::kernel::request_guest_call(CLEAR_COLLECT);
                    "all COLLECT flags cleared".into()
                }
                9 => {
                    st.open = false;
                    String::new()
                }
                _ => String::new(),
            };
        }
    }
    let target = chars.get(st.pick).map(|&t| name(m, t)).unwrap_or_else(|| "-".into());
    let lines = [
        format!("Switch character: < {target} >  ({}/{})", st.pick + 1, chars.len()),
        "Back to the original character".to_string(),
        format!("Unlimited health: {}", if st.god { "ON" } else { "OFF" }),
        "Restore 30% health (matty debug)".to_string(),
        "Set COLLECT flags (matty debug)".to_string(),
        "Clear COLLECT flags (matty debug)".to_string(),
        format!("Unlimited devil gauge: {}", if st.devil { "ON" } else { "OFF" }),
        format!("Team swap when switching: {}", if st.swap_team { "ON" } else { "OFF" }),
        format!("Position probe: {}", if st.probe { "ON" } else { "OFF" }),
        "Close".to_string(),
    ];
    out.push_str("MOD MENU (Devil Within, branch devil/debug)\n");
    for (i, l) in lines.iter().enumerate() {
        out.push_str(if i == st.cur { "> " } else { "  " });
        out.push_str(l);
        out.push('\n');
    }
    if let Some((_, _, t, _, _)) = st.switched {
        out.push_str(&format!("controlling: {}\n", name(m, t)));
    }
    if st.probe && cd != 0 {
        // Candidate position fields of the character in control: the ones that follow its
        // movement decide teleport / no-clip (DevilWithin.md).
        let f = |a: u32| f32::from_bits(word(m, a));
        out.push_str(&format!(
            "probe cd={cd:#x}: mtx+0x30 {:.1} {:.1} {:.1} | yaw {:.3} | obj+0x16C {:.1} {:.1} {:.1}\n",
            f(cd + 0xA0),
            f(cd + 0xA4),
            f(cd + 0xA8),
            f(cd + 0x110),
            f(p + 0x16C),
            f(p + 0x170),
            f(p + 0x174)
        ));
    }
    if !st.last.is_empty() {
        out.push_str(&format!("{}\n", st.last));
    }
    out.push_str("--\nhold SELECT (Tab) +  Up/Down: entry   Left/Right: choose   CIRCLE (C): run   L1: close\n\n");
}
