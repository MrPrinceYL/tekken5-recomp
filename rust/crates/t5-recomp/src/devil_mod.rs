//! Devil Within mod menu (branch `devil/debug` only, opt-in with T5_DW_DEBUG): an in-game menu
//! that changes guest data and calls guest functions. Not part of the 1:1 recompilation.
//!
//! SELECT held + L1 opens / closes it; with SELECT held: Up / Down entry, Left / Right value,
//! CIRCLE run / toggle. Structures (DevilWithin.md, "Mod-Menü"):
//!
//! - Characters ("man", 0x148400): tasks of type 1000 (+0x3C) in the task pool 0x2A9A30.
//!   +0x140 input record index, +0x144 character data index, +0x148 AI, +0x154 bit 2 = AI
//!   controlled (set at creation), +0x160 profile id, +0x162 pad port.
//! - Input records (0x15C6A0): pool **0x42B6FC, stride [0x42B6F0], count [0x42B6EA] (short);
//!   +0 (short) controller: 0 = pad 1 (0x15C4E0(0)), < 0 = the AI's pad buffer [0x41BED0],
//!   other = no input.
//! - Player ("ManMgr", 0x146FB0): manager [0x41BB10], current entry index at +0x140, entries
//!   **0x42B66C stride [0x42B660] count [0x42B65A] (short); entry +0 = the player character
//!   (0x147A98; camera, HUD and the enemies' target follow it; 0x1471F8 sets it).
//! - Character data (0x16EF88): **0x4804B4 stride [0x4804A8] count [0x4804A2] (short); life
//!   manager at +0x3004: life +0x301C, max +0x3020 (floats, 0x16FCE0), team +0x300A (short,
//!   tsEHpManGet/SetTeamID 0x14A4E0 / 0x14A460), devil gauge +0x3024, max +0x3028 (floats,
//!   tsEHpManGet/SetDGauge 0x14ACB0 / 0x14AD50 -> 0x16FDF0), position matrix +0x70
//!   (tsEHpManGetPosMtxPtr 0x14A138), yaw +0x110 (0x14A738).
//! - Character profiles (0x120358): table [[0x41B450] + 0x48], count [[0x41B450] + 0x44],
//!   0x80 bytes each: +4 id, +0x40 name (`chr_*`, common/char_prof.bin).
//! - Hero record (how the game saves and restores the hero, 0x135180 / 0x135B20): 0x146D98(man)
//!   writes the man's state to 0x42B610 (0x44 bytes, +4 profile id, position, yaw, life, devil
//!   gauge; checked by 0x146D10); 0x1203F0(1, profile) loads the profile's `list/` (model,
//!   motions, AI); 0x148BC0(record, 0) creates the man from the record as the player.

use ps2_runtime::bus::{ram_read, ram_write};
use ps2_runtime::kernel::{request_guest_call, request_thread_call};
use ps2_runtime::Machine;
use std::sync::Mutex;

const TASK_POOL: u32 = 0x002A_9A30;
/// Task type of a character ("man": 0x148400 passes 1000 to the task allocator 0x123288, which
/// stores it at +0x3C; the tsEHpMan* functions check it).
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
const PROFILES: u32 = 0x0041_B450;
const HERO_RECORD: u32 = 0x0042_B610;
const SAVE_HERO: u32 = 0x0014_6D98;
const CREATE_HERO: u32 = 0x0014_8BC0;
const TASK_KILL: u32 = 0x0020_B428;
/// Pack load slots (0x10C3A8): [0x41AF98] + slot * 0x4C; free while +0x141 (loaded, 0x10A060)
/// and +0x144 (pack) are 0. 16 slots.
const LOAD_SLOTS: u32 = 0x0041_AF98;

/// Whether the model of a profile is in a loaded pack: the slots' BDPK packs (+0x144: handle -> header
/// with the entry count at +4, entries of 0x80 bytes from +0x80, name at +0x14; 0x10A5A8).
/// The profile `chr_X[_lvN]` uses the model `obj_obj_X` (common/char_model_all.txt). Retail
/// discs cannot load other characters' data: the list loader 0x1203F0 opens
/// `host0:romdata/list/<name>.txt` on the developers' PC; character data comes only from the
/// packs of the area being played (Jin and Devil Jin from common/base.pak).
fn model_loaded(m: &Machine, profile: &str) -> bool {
    let base = profile.strip_prefix("chr_").unwrap_or(profile);
    let base = match base.rfind("_lv") {
        Some(i) if base[i + 3..].chars().all(|c| c.is_ascii_digit()) => &base[..i],
        _ => base,
    };
    let base = base.strip_suffix(|c: char| c.is_ascii_uppercase()).map(|b| b.trim_end_matches('_')).unwrap_or(base);
    let want = format!("obj_obj_{base}");
    let slots = word(m, LOAD_SLOTS);
    (0..16).any(|s| {
        // +0x144 is a handle whose first word points at the pack data (0x109F80).
        let h = word(m, slots + s * 0x4C + 0x144);
        let pak = if h != 0 { word(m, h) } else { 0 };
        if pak == 0 || word(m, pak) != 0x4B50_4442 {
            return false;
        }
        let n = word(m, pak + 4).min(4096);
        (0..n).any(|i| {
            let e = pak + 0x80 * (i + 1);
            half(m, e) as u16 == 0x4442 && cstr(m, e + 0x14) == want
        })
    })
}

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

/// The living characters (tasks of the man type).
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
    word(m, word(m, MAN_POOL)).wrapping_add(word(m, MAN_STRIDE).wrapping_mul(idx as u32))
}

fn input_record(m: &Machine, t: u32) -> u32 {
    let i = word(m, t + 0x140) as i32;
    if i < 0 || half(m, INPUT_COUNT) < 1 {
        return 0;
    }
    word(m, word(m, INPUT_POOL)).wrapping_add(word(m, INPUT_STRIDE).wrapping_mul(i as u32))
}

fn char_data(m: &Machine, t: u32) -> u32 {
    let i = word(m, t + 0x144) as i32;
    if i < 0 || half(m, CHR_COUNT) < 1 {
        return 0;
    }
    word(m, word(m, CHR_POOL)).wrapping_add(word(m, CHR_STRIDE).wrapping_mul(i as u32))
}

/// All character profiles: (id, address, name).
fn profiles(m: &Machine) -> Vec<(u32, u32, String)> {
    let base = word(m, PROFILES);
    if base == 0 {
        return Vec::new();
    }
    let (table, count) = (word(m, base + 0x48), word(m, base + 0x44) as i32);
    if table == 0 || !(1..=256).contains(&count) {
        return Vec::new();
    }
    (0..count as u32).map(|i| table + i * 0x80).map(|p| (word(m, p + 4), p, cstr(m, p + 0x40))).collect()
}

/// A character's display name: its profile's name, AI or player.
fn name(m: &Machine, t: u32) -> String {
    let id = half(m, t + 0x160) as u16 as u32;
    let prof = profiles(m).into_iter().find(|p| p.0 == id).map(|p| p.2).unwrap_or_else(|| format!("profile {id:#x}"));
    let ai = if word(m, t + 0x154) & 2 != 0 { "AI" } else { "player" };
    format!("{prof} ({ai})")
}

/// The player character: the ManMgr entry if it is a living man, else the man on pad 1.
fn player(m: &Machine) -> u32 {
    let slot = player_slot(m);
    let p = if slot != 0 { word(m, slot) } else { 0 };
    if p != 0 && alive(m, p) {
        return p;
    }
    characters(m)
        .into_iter()
        .find(|&t| {
            let r = input_record(m, t);
            r != 0 && half(m, r) == 0
        })
        .unwrap_or(0)
}

/// Steps of "Become": the game saves the hero record (thread call SAVE_HERO, record magic
/// cleared first so its arrival is visible); then the old man is removed and the new one created
/// from the record with the new profile id (the creation 0x148968 -> 0x148400 fails while a
/// hero exists, headless test 18.10.). If no new man appears, the old profile is created again
/// from the same record.
#[derive(Clone, Copy, PartialEq)]
enum Become {
    Idle,
    Saved { old: u32, id: u32, waited: u32 },
    Created { old_id: u32, id: u32, waited: u32 },
}

#[derive(Clone, Copy, PartialEq)]
enum Entry {
    Become,
    Switch,
    Back,
    God,
    Devil,
    Heal,
    TeamSwap,
    SetCollect,
    ClearCollect,
    Probe,
    SavePos,
    LoadPos,
    ToSpawn,
    Close,
}

const ENTRIES: [Entry; 14] = [
    Entry::Become,
    Entry::Switch,
    Entry::Back,
    Entry::God,
    Entry::Devil,
    Entry::Heal,
    Entry::TeamSwap,
    Entry::SetCollect,
    Entry::ClearCollect,
    Entry::Probe,
    Entry::SavePos,
    Entry::LoadPos,
    Entry::ToSpawn,
    Entry::Close,
];

struct State {
    open: bool,
    cur: usize,
    prev_held: u32,
    /// Index into the character list of the switch target.
    pick: usize,
    /// Index into the profile list for "Become".
    profile: usize,
    become_step: Become,
    /// Active switch: (original player, its controller, target, the target's controller,
    /// the target's team before a swap).
    switched: Option<(u32, i16, u32, i16, Option<i16>)>,
    god: bool,
    devil: bool,
    /// Give a switched-to character the player's team (it fights the others).
    swap_team: bool,
    probe: bool,
    /// Saved position (man +0x90..0x98, the floats the hero record saves).
    saved_pos: Option<[u32; 3]>,
    /// Where each man was when the mod first saw it.
    spawn: std::collections::HashMap<u32, [u32; 3]>,
    last: String,
}

fn position(m: &Machine, man: u32) -> [u32; 3] {
    [word(m, man + 0x90), word(m, man + 0x94), word(m, man + 0x98)]
}

fn set_position(m: &mut Machine, man: u32, pos: [u32; 3]) {
    for (i, v) in pos.iter().enumerate() {
        ram_write(m, man + 0x90 + 4 * i as u32, 4, *v as u64);
    }
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Control `target`: pad 1 drives it, the current player gets no input, the ManMgr entry
/// (camera, HUD, enemy target) points at it.
fn switch_to(m: &mut Machine, st: &mut State, target: u32) -> String {
    restore(m, st);
    let slot = player_slot(m);
    let player = player(m);
    if slot == 0 || player == 0 {
        return "no player in this scene".into();
    }
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

/// The man of profile `id` other than `old`, if one exists.
fn man_of(m: &Machine, id: u32, old: u32) -> Option<u32> {
    characters(m).into_iter().find(|&t| t != old && half(m, t + 0x160) as u16 as u32 == id)
}

/// Advance "Become" by one step per vertical blank.
fn become_step(m: &mut Machine, st: &mut State) {
    match st.become_step {
        Become::Idle => {}
        Become::Saved { old, id, waited } => {
            if half(m, HERO_RECORD) as u16 != 0x33AC || half(m, HERO_RECORD + 0x28) as u16 != 0x3832 {
                st.become_step = if waited > 120 {
                    st.last = "become: the game did not write the hero record".into();
                    Become::Idle
                } else {
                    Become::Saved { old, id, waited: waited + 1 }
                };
                return;
            }
            let old_id = word(m, HERO_RECORD + 4);
            ram_write(m, HERO_RECORD + 4, 4, id as u64);
            if alive(m, old) {
                request_thread_call(TASK_KILL, &[TASK_POOL as u64, old as u64]);
            }
            request_thread_call(CREATE_HERO, &[HERO_RECORD as u64, 0]);
            st.become_step = Become::Created { old_id, id, waited: 0 };
        }
        Become::Created { old_id, id, waited } => match man_of(m, id, 0) {
            Some(new) => {
                ps2_runtime::diag::log(format!("devil mod: became profile {id:#x} ({new:#x})"));
                st.last = format!("now playing as {}", name(m, new));
                st.become_step = Become::Idle;
            }
            None if waited > 60 => {
                // Give the old character back from the same record.
                ram_write(m, HERO_RECORD + 4, 4, old_id as u64);
                request_thread_call(CREATE_HERO, &[HERO_RECORD as u64, 0]);
                st.last = "become: the game could not create that character here; old one restored".into();
                ps2_runtime::diag::log(format!("devil mod: become profile {id:#x} failed, restoring {old_id:#x}"));
                st.become_step = Become::Idle;
            }
            None => st.become_step = Become::Created { old_id, id, waited: waited + 1 },
        },
    }
}

/// Every vertical blank while Devil Within runs: input, effects, menu text.
pub fn update(m: &mut Machine, out: &mut String) {
    let mut g = STATE.lock().unwrap();
    let st = g.get_or_insert_with(|| State {
        open: false,
        cur: 0,
        prev_held: 0,
        pick: 0,
        profile: 0,
        become_step: Become::Idle,
        switched: None,
        god: false,
        devil: false,
        swap_team: true,
        probe: false,
        saved_pos: None,
        spawn: std::collections::HashMap::new(),
        last: String::new(),
    });
    become_step(m, st);
    let living = characters(m);
    st.spawn.retain(|t, _| living.contains(t));
    for &t in &living {
        let pos = position(m, t);
        st.spawn.entry(t).or_insert(pos);
    }
    // A switched-to character that died or was removed: give control back.
    if let Some((_, _, target, _, _)) = st.switched {
        if !alive(m, target) {
            restore(m, st);
            st.last = "controlled character is gone, back to the original".into();
        }
    }
    // Unlimited health / devil gauge for the character in control.
    let p = player(m);
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
    let profs = profiles(m);
    if st.pick >= chars.len() {
        st.pick = 0;
    }
    if st.profile >= profs.len() {
        st.profile = 0;
    }
    let entry = ENTRIES[st.cur];
    if held & SELECT != 0 {
        if new & UP != 0 {
            st.cur = (st.cur + ENTRIES.len() - 1) % ENTRIES.len();
        }
        if new & DOWN != 0 {
            st.cur = (st.cur + 1) % ENTRIES.len();
        }
        let step = |i: &mut usize, n: usize| {
            if n > 0 {
                if new & RIGHT != 0 {
                    *i = (*i + 1) % n;
                }
                if new & LEFT != 0 {
                    *i = (*i + n - 1) % n;
                }
            }
        };
        match entry {
            Entry::Switch => step(&mut st.pick, chars.len()),
            Entry::Become => step(&mut st.profile, profs.len()),
            _ => {}
        }
        if new & CIRCLE != 0 {
            st.last = match entry {
                Entry::Become => match (profs.get(st.profile), player(m)) {
                    (Some((_, _, pname)), _) if !model_loaded(m, pname) => {
                        format!("{pname} is not loaded in this area (retail data comes only from the area's packs)")
                    }
                    (Some((id, _, pname)), old) if old != 0 && st.become_step == Become::Idle => {
                        restore(m, st);
                        ram_write(m, HERO_RECORD, 4, 0);
                        request_thread_call(SAVE_HERO, &[old as u64]);
                        st.become_step = Become::Saved { old, id: *id, waited: 0 };
                        format!("becoming {pname} ...")
                    }
                    (None, _) => "no profiles loaded".into(),
                    _ => "no player in this scene".into(),
                },
                Entry::Switch => match chars.get(st.pick) {
                    Some(&t) => switch_to(m, st, t),
                    None => "no characters in this scene".into(),
                },
                Entry::Back => {
                    if st.switched.is_some() {
                        restore(m, st);
                        "back to the original character".into()
                    } else {
                        "not switched".into()
                    }
                }
                Entry::God => {
                    st.god = !st.god;
                    format!("unlimited health {}", if st.god { "on" } else { "off" })
                }
                Entry::Devil => {
                    st.devil = !st.devil;
                    format!("unlimited devil gauge {}", if st.devil { "on" } else { "off" })
                }
                Entry::TeamSwap => {
                    st.swap_team = !st.swap_team;
                    format!("team swap when taking over {}", if st.swap_team { "on" } else { "off" })
                }
                Entry::Probe => {
                    st.probe = !st.probe;
                    format!("position probe {}", if st.probe { "on" } else { "off" })
                }
                Entry::Heal => {
                    request_guest_call(HEAL_30);
                    "restored 30% health (game function 0x1D92B0)".into()
                }
                Entry::SetCollect => {
                    request_guest_call(SET_COLLECT);
                    "all COLLECT flags set (saved with the game!)".into()
                }
                Entry::ClearCollect => {
                    request_guest_call(CLEAR_COLLECT);
                    "all COLLECT flags cleared".into()
                }
                Entry::SavePos => match player(m) {
                    0 => "no player in this scene".into(),
                    p => {
                        st.saved_pos = Some(position(m, p));
                        "position saved".into()
                    }
                },
                Entry::LoadPos => match (player(m), st.saved_pos) {
                    (0, _) => "no player in this scene".into(),
                    (_, None) => "no position saved".into(),
                    (p, Some(pos)) => {
                        set_position(m, p, pos);
                        "teleported to the saved position".into()
                    }
                },
                Entry::ToSpawn => match player(m) {
                    0 => "no player in this scene".into(),
                    p => match st.spawn.get(&p).copied() {
                        Some(pos) => {
                            set_position(m, p, pos);
                            "teleported to where this character appeared".into()
                        }
                        None => "spawn point unknown".into(),
                    },
                },
                Entry::Close => {
                    st.open = false;
                    String::new()
                }
            };
        }
    }
    let target = chars.get(st.pick).map(|&t| name(m, t)).unwrap_or_else(|| "-".into());
    let prof = profs
        .get(st.profile)
        .map(|p| format!("{} ({:#x}){}", p.2, p.0, if model_loaded(m, &p.2) { "" } else { " [not loaded here]" }))
        .unwrap_or_else(|| "-".into());
    let on = |b: bool| if b { "ON" } else { "OFF" };
    out.push_str("MOD MENU (Devil Within, branch devil/debug)\n");
    for (i, e) in ENTRIES.iter().enumerate() {
        out.push_str(if i == st.cur { "> " } else { "  " });
        out.push_str(&match e {
            Entry::Become => format!("Become: < {prof} >  ({}/{})", st.profile + 1, profs.len()),
            Entry::Switch => format!("Take over: < {target} >  ({}/{})", st.pick + 1, chars.len()),
            Entry::Back => "Back to the original character".into(),
            Entry::God => format!("Unlimited health: {}", on(st.god)),
            Entry::Devil => format!("Unlimited devil gauge: {}", on(st.devil)),
            Entry::Heal => "Restore 30% health (matty debug)".into(),
            Entry::TeamSwap => format!("Team swap when taking over: {}", on(st.swap_team)),
            Entry::SetCollect => "Set COLLECT flags (matty debug)".into(),
            Entry::ClearCollect => "Clear COLLECT flags (matty debug)".into(),
            Entry::Probe => format!("Position probe: {}", on(st.probe)),
            Entry::SavePos => "Save position".into(),
            Entry::LoadPos => "Teleport to saved position".into(),
            Entry::ToSpawn => "Teleport to spawn".into(),
            Entry::Close => "Close".into(),
        });
        out.push('\n');
    }
    if p != 0 {
        out.push_str(&format!("player: {}\n", name(m, p)));
    }
    if let Some((_, _, t, _, _)) = st.switched {
        out.push_str(&format!("controlling: {}\n", name(m, t)));
    }
    if st.probe && cd != 0 {
        // Candidate position fields of the character in control: the ones that follow its
        // movement decide teleport / no-clip (DevilWithin.md).
        let f = |a: u32| f32::from_bits(word(m, a));
        out.push_str(&format!(
            "probe man={p:#x}: pos {:.1} {:.1} {:.1} (+0x90, what the hero record saves) | cd+0xA0 {:.1} {:.1} {:.1}\n",
            f(p + 0x90),
            f(p + 0x94),
            f(p + 0x98),
            f(cd + 0xA0),
            f(cd + 0xA4),
            f(cd + 0xA8)
        ));
    }
    if !st.last.is_empty() {
        out.push_str(&format!("{}\n", st.last));
    }
    out.push_str("--\nhold SELECT (Tab) +  Up/Down: entry   Left/Right: choose   CIRCLE (C): run   L1: close\n\n");
}
