//! Devil Within mod menu (branch `devil/debug` only, opt-in with T5_DW_DEBUG): an in-game menu
//! that changes guest data and calls guest functions. Not part of the 1:1 recompilation.
//!
//! L3 + R3 opens / closes it; while open the pad drives only the menu (the game reads nothing,
//! `pad::BLOCK`): Up / Down entry, Left / Right value, CIRCLE run / toggle. Structures (DevilWithin.md, "Mod-Menü"):
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
use ps2_runtime::kernel::{request_guest_call, request_thread_call, PREV_RESULT};
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
/// Register a loaded BDPK pack in a load slot (0x109F80): (handle, slot, flag).
const REGISTER_PACK: u32 = 0x0010_A208;
/// Unload a load slot (what the area loader 0x12D7A0 does before reusing one).
const UNLOAD_SLOT: u32 = 0x0010_A808;
/// The slots the mod loads other areas' character packs into: the variant's texture pack, the
/// texture pack of the model's room, then the pack (texture packs first, as the area loader
/// 0x12D7A0 states 5 / 9 does); free in every scene (the game uses 0-6 and 15; RAM 18.10.).
const MOD_VARIANT_SLOT: u32 = 8;
const MOD_TEX_SLOT: u32 = 9;
const MOD_SLOT: u32 = 10;
/// The texture / model counters (0x10B098 / 0x10B528 hand out indices from them; a slot keeps
/// their values from before its load at pack +0xC / +0xE, and unloading it (0x10B1C8 /
/// 0x10B6E0) sets them back: a stack). Unloading the mod's slots after a later load (a room)
/// would put them below that load's indices; then the mod keeps them (memcpy around the unload).
const COUNTERS: u32 = 0x0041_AFD8;
const MEMCPY: u32 = 0x0027_3034;
const COUNTER_SAVE: u32 = PATH_BUFS + 11 * 0x100 + 0x80;
/// The pack files are not loaded by the game's file functions: opening a file while the disc is
/// busy waits in a loop that does not end inside a thread call (the synchronous 0x20EA98 hung the
/// game, headless 18.10.). The mod reads them from the disc image itself (TK5DATA2 directory in
/// RAM: 0x146 entries {checksum, name offset into 0x363D60, offset, size} at 0x362900) into a
/// block of the game's heap (0x20C458(name, size) -> handle {data, size, ..}).
const ALLOC: u32 = 0x0020_C458;
const FILE_DIR: u32 = 0x0036_2900;
const FILE_NAMES: u32 = 0x0036_3D60;
const FILE_COUNT: u32 = 0x146;
/// The heap (0x20C458): *[0x41E6C0] + 0xC = first free block {+0 data, +4 size, +0x20 next}.
/// An allocation it cannot serve ends in an endless loop, so the mod checks first.
const HEAP: u32 = 0x0041_E6C0;
/// The thread calls give the mod no return value: the handle goes through the texture group
/// word (0x2164C0 sets it, the game keeps it 0 outside registrations) into HANDLES.
const SET_GROUP: u32 = 0x0021_64C0;
const GROUP: u32 = 0x003B_8640;
const HANDLES: u32 = PATH_BUFS + 11 * 0x100 + 0x90;
const JIN: u32 = 0x31E;

/// A file of TK5DATA2: (byte offset in the disc image, size).
fn disc_file(m: &Machine, path: &str) -> Option<(u64, u32)> {
    let (lba, _) = m.disc.as_ref()?.find("TK5DATA2.BIN")?;
    (0..FILE_COUNT).map(|i| FILE_DIR + i * 16).find(|&e| cstr(m, FILE_NAMES + word(m, e + 4)) == path).map(|e| {
        (lba as u64 * 2048 + word(m, e + 8) as u64, word(m, e + 12))
    })
}

/// The largest free block of the game's heap.
fn heap_free(m: &Machine) -> u32 {
    let mut b = word(m, word(m, HEAP) + 0xC);
    let mut best = 0;
    for _ in 0..4096 {
        if b == 0 {
            break;
        }
        best = best.max(word(m, b + 4));
        b = word(m, b + 0x20);
    }
    best
}

/// The path buffer of a load slot (0x10C3A8 builds slot s's path at 0x425B80 + s * 0x100).
const PATH_BUFS: u32 = 0x0042_5B80;
/// Build a man's creation info (0x148A70: info, +0 value, profile id; +4 flags = 0).
const MAN_INFO: u32 = 0x0014_8A70;
/// Create a man from (transform, info) through the ManMgr (0x148968 -> 0x148400). Info flag 2
/// (+4) = not the hero: no "hero exists" check, the task is killable (what 0x157A50 clones
/// enemies with, info at man +0x100).
const MAN_CREATE: u32 = 0x0014_8968;
/// Scratch for a spawn: the path buffers of the unused load slots 12 / 13.
const SPAWN_INFO: u32 = PATH_BUFS + 12 * 0x100;
const SPAWN_MAT: u32 = PATH_BUFS + 13 * 0x100;
const SPAWN_PENDING: u32 = 0xDEAD_BEEF;

/// The room pack that holds a character's model, motions and AI (packs checked against the
/// lists, sheets/dw_files.csv + the pack inventory, DevilWithin.md).
fn pack_of(base: &str) -> Option<&'static str> {
    Some(match base {
        "boss1" => "area/lab_A/lab_A_005.pak",
        "boss2" => "area/lab_B/lab_B_005.pak",
        "boss3" => "area/egy_A/egy_A_005.pak",
        "boss5" => "area/blu_B/blu_B_005.pak",
        "tk_standard1" | "tk_standard2" | "tk_speed1" | "tk_power2" => "area/lab_A/lab_A_003.pak",
        "tk_tricky1" | "tk_power1" => "area/lab_B/lab_B_001.pak",
        "mon_standard1" | "mon_standard2" | "mon_speed1" | "mon_tricky1" | "mon_power1" => "area/blu_A/blu_A_001.pak",
        "mon_power2" => "area/blu_A/blu_A_003.pak",
        _ => return None,
    })
}

/// The model base of a profile: `chr_X[_lvN][_A..D]` -> `X`.
fn model_base(profile: &str) -> String {
    let base = profile.strip_prefix("chr_").unwrap_or(profile);
    let base = match base.rfind("_lv") {
        Some(i) if base[i + 3..].chars().all(|c| c.is_ascii_digit()) => &base[..i],
        _ => base,
    };
    base.strip_suffix(|c: char| c.is_ascii_uppercase()).map(|b| b.trim_end_matches('_')).unwrap_or(base).to_string()
}

/// Whether the model of a profile is in a loaded pack: the slots' BDPK packs (+0x144: handle -> header
/// with the entry count at +4, entries of 0x80 bytes from +0x80, name at +0x14; 0x10A5A8).
/// The profile `chr_X[_lvN]` uses the model `obj_obj_X` (common/char_model_all.txt). Retail
/// discs cannot load other characters' data: the list loader 0x1203F0 opens
/// `host0:romdata/list/<name>.txt` on the developers' PC; character data comes only from the
/// packs of the area being played (Jin and Devil Jin from common/base.pak).
fn model_loaded(m: &Machine, profile: &str) -> bool {
    (0..16).any(|s| slot_has_model(m, s, profile))
}

/// Whether load slot `s` holds the model of `profile`.
fn slot_has_model(m: &Machine, s: u32, profile: &str) -> bool {
    slot_has_entry(m, s, &format!("obj_obj_{}", model_base(profile)))
}

/// Whether load slot `s` holds a pack entry named `want`.
fn slot_has_entry(m: &Machine, s: u32, want: &str) -> bool {
    // +0x144 is a handle whose first word points at the pack data (0x109F80).
    let h = word(m, word(m, LOAD_SLOTS) + s * 0x4C + 0x144);
    let pak = if h != 0 { word(m, h) } else { 0 };
    if pak == 0 || word(m, pak) != 0x4B50_4442 {
        return false;
    }
    let n = word(m, pak + 4).min(4096);
    (0..n).any(|i| {
        let e = pak + 0x80 * (i + 1);
        half(m, e) as u16 == 0x4442 && cstr(m, e + 0x14) == want
    })
}

/// The packs that draw `profile` with its textures (variant's texture pack, texture pack of the
/// model's room, pack): the generated table (level / boss variants shift their texture ids by a
/// group, so they need the texture pack of a room where that variant appears, while the model
/// resolves its own ids when it is registered; devil_chars.rs), else the model's own room
/// (variants without textures on the disc; drawn untextured).
struct Packs {
    variant_tex: String,
    model_tex: String,
    pack: String,
    /// The textures it draws with / its model names; empty = all of the pack.
    draw_ids: Vec<u32>,
    model_ids: Vec<u32>,
}

fn char_packs(profile: &str) -> Option<Packs> {
    if let Some(c) = crate::devil_chars::CHAR_PACKS.iter().find(|c| c.profile == profile) {
        return Some(Packs {
            variant_tex: c.variant_tex.into(),
            model_tex: c.model_tex.into(),
            pack: c.pack.into(),
            draw_ids: c.draw_ids.to_vec(),
            model_ids: c.model_ids.to_vec(),
        });
    }
    pack_of(&model_base(profile)).map(|p| {
        let t = p.replace(".pak", "_tex.pak");
        Packs { variant_tex: t.clone(), model_tex: t, pack: p.into(), draw_ids: Vec::new(), model_ids: Vec::new() }
    })
}

/// Rebuild a room's BDPK pack with what one character needs: its model (`obj_obj_<base>`) and
/// everything that is not a model (motions `gym_*`, ...); the room's background (`obj_bg_*`) and
/// the other models (gimmicks, other characters) go. A room pack is ~2.4 MB, mostly those (the
/// heap had too little room for it next to a room, 18.10.). Layout: header 0x80 (+4 count, +8
/// size), 0x80 per entry {"BD", type, .., +8 size, +0x10 offset, +0x14 name}, data 16-aligned.
fn trim_pack(pack: &[u8], model: &str) -> Vec<u8> {
    let rd = |o: usize| u32::from_le_bytes(pack[o..o + 4].try_into().unwrap());
    let n = rd(4) as usize;
    let mut keep = Vec::new();
    for i in 0..n {
        let e = 0x80 * (i + 1);
        if e + 0x80 > pack.len() {
            break;
        }
        let name_end = pack[e + 0x14..e + 0x80].iter().position(|&b| b == 0).unwrap_or(0x6C);
        let name = std::str::from_utf8(&pack[e + 0x14..e + 0x14 + name_end]).unwrap_or("");
        let model_entry = pack[e + 2] == 1 && (name.starts_with("obj_bg_") || name.starts_with("obj_obj_"));
        if !model_entry || name == model {
            keep.push(e);
        }
    }
    let mut out = pack[..0x80].to_vec();
    for &e in &keep {
        out.extend_from_slice(&pack[e..e + 0x80]);
    }
    for (k, &e) in keep.iter().enumerate() {
        let (size, off) = (rd(e + 8) as usize, rd(e + 0x10) as usize);
        out.resize((out.len() + 15) & !15, 0);
        let at = out.len() as u32;
        out.extend_from_slice(&pack[off..(off + size).min(pack.len())]);
        let rec = 0x80 * (k + 1);
        out[rec + 0x10..rec + 0x14].copy_from_slice(&at.to_le_bytes());
    }
    out.resize((out.len() + 15) & !15, 0);
    let (count, total) = (keep.len() as u32, out.len() as u32);
    out[4..8].copy_from_slice(&count.to_le_bytes());
    out[8..12].copy_from_slice(&total.to_le_bytes());
    out
}

/// Keep only the TIM2 pictures with a GIDX in `keep` in the texture bundles (type 2 entries) of
/// a BDPK pack (the texture index counter allows 256 textures: a second room bundle beside the
/// current room's ran past it and the character was not drawn, headless 18.10.).
fn trim_textures(pack: &mut [u8], keep: &[u32]) {
    let rd = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    if keep.is_empty() || pack.len() < 8 {
        return;
    }
    let n = rd(pack, 4) as usize;
    for i in 0..n {
        let e = 0x80 * (i + 1);
        if e + 0x80 > pack.len() || &pack[e..e + 2] != b"BD" || pack[e + 2] != 2 {
            continue;
        }
        let (size, off) = (rd(pack, e + 8) as usize, rd(pack, e + 0x10) as usize);
        if off + size > pack.len() || size < 16 || &pack[off..off + 4] != b"TIM2" {
            continue;
        }
        let nut = pack[off..off + size].to_vec();
        let head = if nut[5] == 1 { 0x80 } else { 0x10 };
        let count = u16::from_le_bytes([nut[6], nut[7]]) as usize;
        let mut out = nut[..head].to_vec();
        let (mut at, mut kept) = (head, 0u16);
        for _ in 0..count {
            if at + 0x30 > nut.len() {
                break;
            }
            let total = rd(&nut, at) as usize;
            let hsize = u16::from_le_bytes([nut[at + 0xC], nut[at + 0xD]]) as usize;
            if total == 0 || at + total > nut.len() {
                break;
            }
            let hdr = &nut[at..at + hsize.min(total)];
            let id = hdr.windows(4).position(|w| w == b"GIDX").and_then(|g| hdr.get(g + 8..g + 12)).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
            if id.is_none_or(|id| keep.contains(&id)) {
                out.extend_from_slice(&nut[at..at + total]);
                kept += 1;
            }
            at += total;
        }
        out[6..8].copy_from_slice(&kept.to_le_bytes());
        pack[off..off + out.len()].copy_from_slice(&out);
        pack[e + 8..e + 12].copy_from_slice(&(out.len() as u32).to_le_bytes());
    }
}

/// The main texture bundle of a texture pack: `area/x/<room>_tex.pak` -> `nut_<room>`.
fn nut_of(tex: &str) -> Option<String> {
    let file = tex.rsplit('/').next()?;
    file.strip_suffix("_tex.pak").map(|r| format!("nut_{r}"))
}

/// Model and textures of `profile` loaded (Jin's come with base.pak).
fn char_ready(m: &Machine, profile: &str) -> bool {
    if !model_loaded(m, profile) {
        return false;
    }
    match char_packs(profile) {
        Some(c) => [c.variant_tex, c.model_tex].iter().filter_map(|x| nut_of(x)).all(|nut| (0..16).any(|s| slot_has_entry(m, s, &nut))),
        None => true,
    }
}

/// Whether a living character other than `except` uses a model of the mod's pack.
fn mod_pack_in_use(m: &Machine, except: u32) -> bool {
    let profs = profiles(m);
    characters(m).into_iter().filter(|&t| t != except).any(|t| {
        let id = half(m, t + 0x160) as u16 as u32;
        profs.iter().filter(|p| p.0 == id).any(|p| {
            let nuts: Vec<String> = char_packs(&p.2).map(|c| [c.variant_tex, c.model_tex].iter().filter_map(|x| nut_of(x)).collect()).unwrap_or_default();
            slot_has_model(m, MOD_SLOT, &p.2)
                || nuts.iter().any(|n| slot_has_entry(m, MOD_TEX_SLOT, n) || slot_has_entry(m, MOD_VARIANT_SLOT, n))
        })
    })
}

/// Pad 1 held buttons (0x2A9890 + 0x50).
const L3: u32 = 0x2;
const R3: u32 = 0x4;
const UP: u32 = 0x10;
const RIGHT: u32 = 0x20;
const DOWN: u32 = 0x40;
const LEFT: u32 = 0x80;
const L2: u32 = 0x100;
const R2: u32 = 0x200;
/// Camera tasks (type 0x3EB, 0x1543C0); their data in the pool **0x42B6B4 (stride [0x42B6A8],
/// count [0x42B6A2], index task +0x140; 0x152B78). Data +0xF0 = target task, +0xF4 = its
/// serial (task +0): 0x154AD8 follows the target's +0x90 and drops a target whose serial no
/// longer matches (a removed character).
const CAMERA_TYPE: i16 = 0x3EB;

fn camera_data(m: &Machine, task: u32) -> u32 {
    let i = word(m, task + 0x140) as i32;
    if i < 0 || half(m, 0x0042_B6A2) < 1 {
        return 0;
    }
    word(m, word(m, 0x0042_B6B4)) + word(m, 0x0042_B6A8) * i as u32
}

/// Camera distance (+0x10) and the two height fields (+0x0C, +0x20; camera = target + these,
/// RAM 18.10.: 4500 / 1000 / 1000 in Lab A, +0x38 = their length).
const CAM_FIELDS: [u32; 3] = [0x10, 0x0C, 0x20];

/// Pull the cameras that follow `man` back for big characters: profile +0x20 is 100 for Jin and
/// the soldiers, 250-500 for the bosses (their body size); factor 1 + (size - 100) / 150. The
/// game's values are kept (`bases`, per camera) and taken anew whenever the game changes them.
/// The game still shortens the camera at walls (+0x38 current length < +0x3C wanted, recomputed
/// every frame): in small rooms a very big character (True Ogre) stays close.
fn scale_cameras(m: &mut Machine, man: u32, bases: &mut std::collections::HashMap<u32, ([f32; 3], [f32; 3])>) {
    let id = half(m, man + 0x160) as u16 as u32;
    let size = profiles(m).iter().find(|p| p.0 == id).map(|p| word(m, p.1 + 0x20)).unwrap_or(100);
    let factor = 1.0 + (size.clamp(100, 1000) as f32 - 100.0) / 150.0;
    let mut t = word(m, TASK_POOL + 8);
    for _ in 0..1024 {
        if t == 0 {
            break;
        }
        if half(m, t + 0x3C) == CAMERA_TYPE {
            let c = camera_data(m, t);
            if c != 0 && word(m, c + 0xF0) == man {
                let now = CAM_FIELDS.map(|o| f32::from_bits(word(m, c + o)));
                let (base, written) = bases.entry(c).or_insert((now, now));
                if now != *written {
                    *base = now;
                }
                let want = base.map(|v| v * factor);
                for (o, v) in CAM_FIELDS.iter().zip(want) {
                    ram_write(m, c + o, 4, v.to_bits() as u64);
                }
                *written = want;
            }
        }
        t = word(m, t + 0x18);
    }
}

/// Point the cameras that followed `from` (or lost their target) at `to`.
fn retarget_cameras(m: &mut Machine, from: u32, to: u32) {
    let mut t = word(m, TASK_POOL + 8);
    for _ in 0..1024 {
        if t == 0 {
            break;
        }
        if half(m, t + 0x3C) == CAMERA_TYPE {
            let c = camera_data(m, t);
            if c != 0 {
                let target = word(m, c + 0xF0);
                if target == 0 || target == from || word(m, target) != word(m, c + 0xF4) {
                    ram_write(m, c + 0xF0, 4, to as u64);
                    ram_write(m, c + 0xF4, 4, word(m, to) as u64);
                }
            }
        }
        t = word(m, t + 0x18);
    }
}
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
    /// Waiting for the character's room pack (loaded in thread context into MOD_SLOT).
    Loading { profile: usize, waited: u32 },
    Saved { old: u32, id: u32, waited: u32 },
    /// Become with a pack to load: the old character is gone, the packs are loading.
    Fetching { id: u32, waited: u32 },
    /// Spawn: waiting for MAN_INFO to fill SPAWN_INFO.
    SpawnInfo { old: u32, id: u32, waited: u32 },
    Created { old_id: u32, id: u32, waited: u32 },
}

/// Loading a character's room pack (and texture pack) into the mod's slots.
struct Fetch {
    /// (slot, byte offset in the image, size, textures to keep), texture packs first.
    files: Vec<(u32, u64, u32, Vec<u32>)>,
    /// The character's model (the pack keeps only it of the models).
    model: String,
    /// The current file, read and trimmed, waiting for its heap block.
    data: Vec<u8>,
    /// The file being loaded; files.len() = all registered.
    k: usize,
    /// Vblanks to wait before the first allocation (the unloads run first).
    delay: u32,
    requested: bool,
}

/// Start loading the packs of `pname`: unload the mod's slots (pack first, the stack order; the
/// counters only go back when nothing was loaded after them), then one file per step.
fn start_fetch(m: &mut Machine, st: &mut State, pname: &str) -> Result<(), String> {
    let Some(c) = char_packs(pname) else {
        return Err(format!("{pname}: no data on the disc"));
    };
    let mut wanted = vec![(MOD_TEX_SLOT, c.model_tex.as_str(), c.model_ids.clone()), (MOD_SLOT, c.pack.as_str(), Vec::new())];
    if c.variant_tex != c.model_tex {
        wanted.insert(0, (MOD_VARIANT_SLOT, c.variant_tex.as_str(), c.draw_ids.clone()));
    } else if !c.model_ids.is_empty() {
        wanted[0].2.extend(c.draw_ids.iter().copied());
    }
    let mut files = Vec::new();
    for (slot, file, keep) in &wanted {
        let Some((off, size)) = disc_file(m, file) else {
            return Err(format!("{pname}: {file} not found in TK5DATA2"));
        };
        files.push((*slot, off, size, keep.clone()));
    }
    let on_top = st.mod_top == Some((word(m, COUNTERS), word(m, COUNTERS + 4)));
    if !on_top {
        request_thread_call(MEMCPY, &[COUNTER_SAVE as u64, COUNTERS as u64, 8]);
    }
    request_thread_call(UNLOAD_SLOT, &[MOD_SLOT as u64]);
    request_thread_call(UNLOAD_SLOT, &[MOD_TEX_SLOT as u64]);
    request_thread_call(UNLOAD_SLOT, &[MOD_VARIANT_SLOT as u64]);
    if !on_top {
        request_thread_call(MEMCPY, &[COUNTERS as u64, COUNTER_SAVE as u64, 8]);
    }
    st.mod_top = None;
    for &(slot, file, _) in &wanted {
        let buf = PATH_BUFS + slot * 0x100;
        for (i, b) in file.bytes().chain(std::iter::once(0)).enumerate() {
            ram_write(m, buf + i as u32, 1, b as u64);
        }
    }
    let model = format!("obj_obj_{}", model_base(pname));
    st.fetch = Some(Fetch { files, model, data: Vec::new(), k: 0, delay: 2, requested: false });
    let names: Vec<_> = wanted.iter().map(|w| (w.0, w.1)).collect();
    ps2_runtime::diag::log(format!("devil mod: loading {names:?} for {pname}"));
    Ok(())
}

/// One step of a fetch: Ok(true) when both packs are registered (or queued for it).
fn drive_fetch(m: &mut Machine, st: &mut State) -> Result<bool, String> {
    let Some(f) = st.fetch.as_mut() else { return Ok(true) };
    if f.delay > 0 {
        f.delay -= 1;
        return Ok(false);
    }
    if f.k == f.files.len() {
        st.fetch = None;
        st.mod_top = Some((word(m, COUNTERS), word(m, COUNTERS + 4)));
        return Ok(true);
    }
    let (slot, off, size, ref keep) = f.files[f.k];
    let keep = keep.clone();
    let out = HANDLES + 4 * (f.k as u32 % 2);
    if !f.requested {
        // Read and trim first: only what the character needs gets a heap block.
        let mut buf = vec![0u8; size as usize];
        if !matches!(m.disc.as_mut().map(|d| d.read_into(off, &mut buf)), Some(Ok(()))) {
            st.fetch = None;
            return Err("could not read the pack from the disc image".into());
        }
        let buf = if slot == MOD_SLOT {
            trim_pack(&buf, &f.model)
        } else {
            // Texture packs hold no models: rebuilt only to drop the cut pictures' space.
            trim_textures(&mut buf, &keep);
            trim_pack(&buf, "")
        };
        let need = (buf.len() as u32 + 0x7F) & !0x7F;
        if heap_free(m) < need {
            st.fetch = None;
            return Err(format!("not enough memory for the pack ({} KB free, {} KB needed)", heap_free(m) / 1024, need / 1024));
        }
        ram_write(m, out, 4, 0);
        request_thread_call(ALLOC, &[(PATH_BUFS + slot * 0x100) as u64, buf.len() as u64]);
        request_thread_call(SET_GROUP, &[PREV_RESULT]);
        request_thread_call(MEMCPY, &[out as u64, GROUP as u64, 4]);
        request_thread_call(SET_GROUP, &[0]);
        f.data = buf;
        f.requested = true;
        return Ok(false);
    }
    let h = word(m, out);
    if h == 0 {
        return Ok(false);
    }
    let data = word(m, h) as usize & 0x1FF_FFFF;
    let buf = std::mem::take(&mut f.data);
    if data + buf.len() > m.ram.len() {
        st.fetch = None;
        return Err("the heap block is outside RAM".into());
    }
    m.ram[data..data + buf.len()].copy_from_slice(&buf);
    request_thread_call(REGISTER_PACK, &[h as u64, slot as u64, 0]);
    f.k += 1;
    f.requested = false;
    // The registration (next frame) shrinks a texture pack to its header (0x10B098 frees the
    // uploaded data): only then is there room for the next file.
    f.delay = 3;
    Ok(false)
}

#[derive(Clone, Copy, PartialEq)]
enum Entry {
    Become,
    Spawn,
    Switch,
    Back,
    NoClip,
    Stage,
    Item,
    Object,
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

const ENTRIES: [Entry; 19] = [
    Entry::Become,
    Entry::Spawn,
    Entry::Switch,
    Entry::Back,
    Entry::NoClip,
    Entry::Stage,
    Entry::Item,
    Entry::Object,
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
    /// The running Become sequence adds the character instead of replacing the player.
    spawning: bool,
    /// The characters before a spawn (the new one is the one not among them).
    before: Vec<u32>,
    /// The counters right after the mod's packs were registered (they are on top while equal).
    mod_top: Option<(u32, u32)>,
    fetch: Option<Fetch>,
    /// The profile whose packs Become loads after removing the old character.
    become_pack: Option<String>,
    /// The player's team (Jin: 0), given to a character made by Become.
    hero_team: i16,
    /// The character made by Become (its team is held at `hero_team`: the game sets the
    /// profile's after creating it).
    become_hero: Option<u32>,
    /// Camera fields as the game set them / as the mod wrote them (scale_cameras).
    cam_bases: std::collections::HashMap<u32, ([f32; 3], [f32; 3])>,
    /// No-clip: the position the mod holds the character in control at.
    noclip: Option<[f32; 3]>,
    /// Index into ITEMS.
    item: usize,
    /// Index into the gimmick types.
    object: usize,
    /// Stage choice 0-4 (the stage select's), then devil_debug::EXTRA_STAGES.
    stage: usize,
    /// Vblanks until stage 0 names Lab A again after starting an extra stage.
    stage_restore: u32,
    /// Active switch: (original player, its controller, target, the target's controller,
    /// the target's team before a swap).
    switched: Option<(u32, i16, u32, i16, Vec<(u32, i16)>)>,
    god: bool,
    devil: bool,
    /// Teams on take over: the controlled character gets the player's team, every other
    /// character (the original player too) the enemies' team; all hit each other like the
    /// player and the enemies do.
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

/// Copies of the position: man +0x90 (translation of its world matrix) is rebuilt every frame
/// from the character data (RAM search 18.10.: Jin's x at cd +0, +0x20, +0xA0, with man
/// +0x90); a move needs all of them.
const POS_COPIES: [u32; 3] = [0x0, 0x20, 0xA0];

fn set_position(m: &mut Machine, man: u32, pos: [u32; 3]) {
    let cd = char_data(m, man);
    for (i, v) in pos.iter().enumerate() {
        ram_write(m, man + 0x90 + 4 * i as u32, 4, *v as u64);
        if cd != 0 {
            for c in POS_COPIES {
                ram_write(m, cd + c + 4 * i as u32, 4, *v as u64);
            }
        }
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
    let mut teams = Vec::new();
    if st.swap_team {
        let dp = char_data(m, player);
        if dp != 0 {
            let ours = half(m, dp + 0x300A);
            let theirs = characters(m)
                .into_iter()
                .filter(|&t| t != player && char_data(m, t) != 0)
                .map(|t| half(m, char_data(m, t) + 0x300A))
                .find(|&t| t != ours)
                .unwrap_or(1 - ours);
            for t in characters(m) {
                let d = char_data(m, t);
                if d == 0 {
                    continue;
                }
                teams.push((t, half(m, d + 0x300A)));
                let team = if t == target { ours } else { theirs };
                ram_write(m, d + 0x300A, 2, team as u16 as u64);
            }
        }
    }
    st.switched = Some((player, cp, target, ct, teams));
    retarget_cameras(m, player, target);
    ps2_runtime::diag::log(format!("devil mod: control {target:#x} (was {player:#x})"));
    format!("now controlling {}", name(m, target))
}

/// Undo a switch (if the original player still exists).
fn restore(m: &mut Machine, st: &mut State) {
    let Some((player, cp, target, ct, teams)) = st.switched.take() else { return };
    for (t, team) in teams {
        let d = if alive(m, t) { char_data(m, t) } else { 0 };
        if d != 0 {
            ram_write(m, d + 0x300A, 2, team as u16 as u64);
        }
    }
    if alive(m, target) {
        let rt = input_record(m, target);
        if rt != 0 {
            ram_write(m, rt, 2, ct as u16 as u64);
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
        retarget_cameras(m, target, player);
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
        Become::Loading { profile, waited } => {
            let profs = profiles(m);
            let Some((id, _, pname)) = profs.get(profile).cloned() else {
                st.become_step = Become::Idle;
                return;
            };
            if st.fetch.is_some() {
                // Spawn: the packs load while the player stays.
                if let Err(e) = drive_fetch(m, st) {
                    st.last = e;
                    st.become_step = Become::Idle;
                } else {
                    st.become_step = Become::Loading { profile, waited: waited + 1 };
                }
                return;
            }
            if st.become_pack.is_some() && !st.spawning {
                // Become: remove the old character first (its pack may be the one replaced).
                let old = player(m);
                if old == 0 {
                    st.last = "no player in this scene".into();
                    st.become_step = Become::Idle;
                    return;
                }
                restore(m, st);
                ram_write(m, HERO_RECORD, 4, 0);
                request_thread_call(SAVE_HERO, &[old as u64]);
                st.become_step = Become::Saved { old, id, waited: 0 };
                return;
            }
            if char_ready(m, &pname) {
                let old = player(m);
                if old == 0 {
                    st.last = "no player in this scene".into();
                    st.become_step = Become::Idle;
                    return;
                }
                st.before = characters(m);
                if st.spawning {
                    ram_write(m, SPAWN_INFO + 4, 4, SPAWN_PENDING as u64);
                    request_thread_call(MAN_INFO, &[SPAWN_INFO as u64, 0, id as u64]);
                    st.become_step = Become::SpawnInfo { old, id, waited: 0 };
                    return;
                }
                restore(m, st);
                ram_write(m, HERO_RECORD, 4, 0);
                request_thread_call(SAVE_HERO, &[old as u64]);
                st.become_step = Become::Saved { old, id, waited: 0 };
            } else if waited > 300 {
                st.last = format!("{pname}: its pack could not be loaded");
                st.become_step = Become::Idle;
            } else {
                st.become_step = Become::Loading { profile, waited: waited + 1 };
            }
        }
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
            // The new hero gets its profile's team (an enemy's: it would hit neither enemies
            // nor objects, headless 18.10.); it takes over the old player's.
            let cd = char_data(m, old);
            if cd != 0 {
                st.hero_team = half(m, cd + 0x300A);
            }
            if alive(m, old) {
                request_thread_call(TASK_KILL, &[TASK_POOL as u64, old as u64]);
            }
            if let Some(pname) = st.become_pack.take() {
                // The old character is removed with this batch; then its pack can go.
                match start_fetch(m, st, &pname) {
                    Ok(()) => st.become_step = Become::Fetching { id, waited: 0 },
                    Err(e) => {
                        st.last = e;
                        ram_write(m, HERO_RECORD + 4, 4, JIN as u64);
                        request_thread_call(CREATE_HERO, &[HERO_RECORD as u64, 0]);
                        st.become_step = Become::Idle;
                    }
                }
                return;
            }
            request_thread_call(CREATE_HERO, &[HERO_RECORD as u64, 0]);
            st.become_step = Become::Created { old_id, id, waited: 0 };
        }
        Become::Fetching { id, waited } => {
            let done = drive_fetch(m, st);
            let profs = profiles(m);
            let pname = profs.iter().find(|p| p.0 == id).map(|p| p.2.clone()).unwrap_or_default();
            match done {
                Ok(true) if char_ready(m, &pname) => {
                    request_thread_call(CREATE_HERO, &[HERO_RECORD as u64, 0]);
                    st.become_step = Become::Created { old_id: JIN, id, waited: 0 };
                }
                Ok(_) if waited < 600 => st.become_step = Become::Fetching { id, waited: waited + 1 },
                res => {
                    // Back to Jin (his model is in base.pak, always loaded).
                    st.last = match res {
                        Err(e) => e,
                        _ => format!("{pname}: its pack could not be loaded"),
                    };
                    st.fetch = None;
                    ram_write(m, HERO_RECORD + 4, 4, JIN as u64);
                    request_thread_call(CREATE_HERO, &[HERO_RECORD as u64, 0]);
                    st.become_step = Become::Idle;
                }
            }
        }
        Become::SpawnInfo { old, id, waited } => {
            if word(m, SPAWN_INFO + 4) == SPAWN_PENDING {
                st.become_step = if waited > 120 {
                    st.last = "spawn: the game did not build the info".into();
                    Become::Idle
                } else {
                    Become::SpawnInfo { old, id, waited: waited + 1 }
                };
                return;
            }
            // +0 and the flags like an enemy of the scene (its info at man +0x100), else flag 2.
            let enemy = st.before.iter().copied().find(|&t| t != old && alive(m, t) && word(m, t + 0x104) & 2 != 0);
            let (v0, flags) = match enemy {
                Some(e) => (word(m, e + 0x100), word(m, e + 0x104)),
                None => (0, 2),
            };
            ram_write(m, SPAWN_INFO, 4, v0 as u64);
            ram_write(m, SPAWN_INFO + 4, 4, flags as u64);
            // Transform: identity, translation row = 800 units (about one character length x4) along x.
            let pos = position(m, old).map(f32::from_bits);
            let one = 1.0f32.to_bits() as u64;
            for i in 0..16u32 {
                ram_write(m, SPAWN_MAT + i * 4, 4, if i % 5 == 0 { one } else { 0 });
            }
            for (i, v) in [pos[0] + 800.0, pos[1], pos[2]].iter().enumerate() {
                ram_write(m, SPAWN_MAT + 0x30 + 4 * i as u32, 4, v.to_bits() as u64);
            }
            request_thread_call(MAN_CREATE, &[SPAWN_MAT as u64, SPAWN_INFO as u64]);
            st.become_step = Become::Created { old_id: 0, id, waited: 0 };
        }
        Become::Created { old_id, id, waited } if st.spawning => {
            let before = st.before.clone();
            match characters(m).into_iter().find(|t| !before.contains(t)) {
                Some(new) => {
                    st.last = spawned(m, new, &before);
                    st.become_step = Become::Idle;
                }
                None if waited > 60 => {
                    st.last = "spawn: the game could not create that character here".into();
                    st.become_step = Become::Idle;
                }
                None => st.become_step = Become::Created { old_id, id, waited: waited + 1 },
            }
        }
        Become::Created { old_id, id, waited } => match man_of(m, id, 0) {
            Some(new) => {
                retarget_cameras(m, 0, new);
                st.become_hero = Some(new);
                ps2_runtime::diag::log(format!("devil mod: became profile {id:#x} ({new:#x})"));
                st.last = format!("now playing as {}", name(m, new));
                st.become_step = Become::Idle;
            }
            None if waited > 60 => {
                // Give the old character back from the same record (Jin if its model is gone).
                let old_loaded = profiles(m).iter().any(|p| p.0 == old_id && char_ready(m, &p.2));
                let old_id = if old_loaded { old_id } else { JIN };
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

/// A character MAN_CREATE added: without an enemy to copy the info from, put it on the team
/// opposite the player's.
fn spawned(m: &mut Machine, new: u32, before: &[u32]) -> String {
    let has_enemy = before.iter().any(|&t| alive(m, t) && word(m, t + 0x104) & 2 != 0);
    let old = player(m);
    let (dn, dp) = (char_data(m, new), if old != 0 { char_data(m, old) } else { 0 });
    if !has_enemy && dn != 0 && dp != 0 {
        ram_write(m, dn + 0x300A, 2, (1 - half(m, dp + 0x300A)) as u16 as u64);
    }
    ps2_runtime::diag::log(format!(
        "devil mod: spawned {new:#x} ({}) at {:?}, player at {:?}; profiles {:?}",
        name(m, new),
        position(m, new).map(f32::from_bits),
        position(m, old).map(f32::from_bits),
        profiles(m).iter().map(|p| p.2.clone()).collect::<Vec<_>>()
    ));
    format!("spawned {}", name(m, new))
}

/// Items as the game drops them from defeated enemies (0x148F20): gimmick 0x1C6D88(parent, type,
/// matrix) with the room task the character stands in as parent (man +0xEC); gold gets its
/// amount from 0x1C6190(item, min, max). Types: 0x873 gold, 0x876 / 0x877 life S / M (table
/// 0x2CECF8), 0x878 life L (in that table, never dropped: the drop picks entry 0 or 1, and no
/// room places it).
const MAKE_GIMMICK: u32 = 0x001C_6D88;
const GOLD_AMOUNT: u32 = 0x001C_6190;
const ITEMS: [(u32, &str); 4] = [
    (0x873, "Gold (100-500)"),
    (0x876, "Life S"),
    (0x877, "Life M"),
    (0x878, "Life L (never dropped or placed in the game)"),
];

/// The gimmick types (0x1C6EF0): table **0x2D3158, +8 count, entries from +0xC {+0 type,
/// +8 a, +0xA b, ..} of 12 + 4a + 8b bytes. No names (they come from gim.db placements).
const GIMMICK_TYPES: u32 = 0x002D_3158;

fn gimmick_types(m: &Machine) -> Vec<u32> {
    let base = match word(m, GIMMICK_TYPES) {
        0 => return Vec::new(),
        h => word(m, h),
    };
    let n = word(m, base + 8).min(1024);
    let mut e = base + 0xC;
    (0..n)
        .map(|_| {
            let id = word(m, e);
            e += 12 + 4 * half(m, e + 8) as u16 as u32 + 8 * half(m, e + 0xA) as u16 as u32;
            id
        })
        .collect()
}

/// Spawn gimmick `kind` 300 units in front of player `p`, child of its room task (man +0xEC).
fn spawn_gimmick(m: &mut Machine, p: u32, kind: u32) -> Result<(), String> {
    if p == 0 {
        return Err("no player in this scene".into());
    }
    let room = word(m, p + 0xEC);
    if room == 0 {
        return Err("the character stands in no room task".into());
    }
    let pos = position(m, p).map(f32::from_bits);
    let one = 1.0f32.to_bits() as u64;
    for i in 0..16u32 {
        ram_write(m, SPAWN_MAT + i * 4, 4, if i % 5 == 0 { one } else { 0 });
    }
    for (i, v) in [pos[0] + 300.0, pos[1], pos[2]].iter().enumerate() {
        ram_write(m, SPAWN_MAT + 0x30 + 4 * i as u32, 4, v.to_bits() as u64);
    }
    request_thread_call(MAKE_GIMMICK, &[room as u64, kind as u64, SPAWN_MAT as u64]);
    Ok(())
}

/// Start a stage (0x10D680: root task, stage); the root task at 0x2AAAF4.
const START_STAGE: u32 = 0x0010_D680;
const ROOT_TASK: u32 = 0x002A_AAF4;
const STAGE_CHOICES: usize = 5 + crate::devil_debug::EXTRA_STAGES.len();

fn stage_name(m: &Machine, i: usize) -> String {
    use crate::devil_debug::{EXTRA_STAGES, STAGE_COUNT, STAGE_NAMES};
    if i as u32 >= STAGE_COUNT {
        return EXTRA_STAGES[i - STAGE_COUNT as usize].label.into();
    }
    // Entry 0 may name an extra stage for a moment: label by index.
    let n = cstr(m, word(m, STAGE_NAMES + 4 * i as u32));
    if i == 0 && EXTRA_STAGES.iter().any(|e| e.name == Some(n.as_str())) { "lab_A".into() } else { n }
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
        spawning: false,
        before: Vec::new(),
        noclip: None,
        mod_top: None,
        fetch: None,
        become_pack: None,
        hero_team: 0,
        become_hero: None,
        cam_bases: std::collections::HashMap::new(),
        stage: 0,
        item: 0,
        object: 0,
        stage_restore: 0,
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
    if let Some(h) = st.become_hero {
        let cd = if alive(m, h) { char_data(m, h) } else { 0 };
        if cd == 0 {
            st.become_hero = None;
        } else if half(m, cd + 0x300A) != st.hero_team && st.switched.is_none() {
            ram_write(m, cd + 0x300A, 2, st.hero_team as u16 as u64);
        }
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
    if p != 0 {
        let mut bases = std::mem::take(&mut st.cam_bases);
        scale_cameras(m, p, &mut bases);
        st.cam_bases = bases;
    }
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
    if st.stage_restore > 0 {
        st.stage_restore -= 1;
        if st.stage_restore == 0 {
            crate::devil_debug::set_stage0(m, None);
        }
    }
    // The real pad (the game gets nothing while the menu is open): buttons, sticks RX RY LX LY.
    let held = m.shared.pad.buttons[0].load(std::sync::atomic::Ordering::Relaxed) as u32;
    let sticks = match m.shared.pad.sticks[0].load(std::sync::atomic::Ordering::Relaxed) {
        0 => 0x8080_8080,
        s => s,
    };
    // No-clip: hold the character in control at the mod's position (no gravity, no walls),
    // left stick moves it along world x / z (0.25 m per frame at full tilt), R2 / L2 up /
    // down; not while the menu is open.
    if let Some(mut pos) = st.noclip {
        if p == 0 {
            st.noclip = None;
        } else {
            if !st.open {
                let axis = |shift: u32| {
                    let v = (((sticks >> shift) & 0xFF) as f32 - 128.0) / 128.0;
                    if v.abs() < 0.2 { 0.0 } else { v }
                };
                pos[0] += axis(16) * 25.0;
                pos[2] += axis(24) * 25.0;
                if held & R2 != 0 {
                    pos[1] += 25.0;
                }
                if held & L2 != 0 {
                    pos[1] -= 25.0;
                }
            }
            set_position(m, p, pos.map(f32::to_bits));
            st.noclip = Some(pos);
        }
    }
    let new = held & !st.prev_held;
    st.prev_held = held;
    if held & (L3 | R3) == L3 | R3 && new & (L3 | R3) != 0 {
        st.open = !st.open;
    }
    ps2_runtime::pad::BLOCK.store(st.open || crate::devil_debug::select_blocks(), std::sync::atomic::Ordering::Relaxed);
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
    {
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
            Entry::Become | Entry::Spawn => step(&mut st.profile, profs.len()),
            Entry::Stage => step(&mut st.stage, STAGE_CHOICES),
            Entry::Item => step(&mut st.item, ITEMS.len()),
            Entry::Object => step(&mut st.object, gimmick_types(m).len()),
            _ => {}
        }
        if new & CIRCLE != 0 {
            st.last = match entry {
                Entry::Become | Entry::Spawn => match (profs.get(st.profile), player(m)) {
                    (Some((_, _, pname)), old) if old != 0 && st.become_step == Become::Idle => {
                        let pname = pname.clone();
                        st.spawning = entry == Entry::Spawn;
                        st.become_pack = None;
                        if char_ready(m, &pname) {
                            st.become_step = Become::Loading { profile: st.profile, waited: 0 };
                            format!("becoming {pname} ...")
                        } else if char_packs(&pname).is_none() {
                            format!("{pname}: no data on the disc")
                        } else if mod_pack_in_use(m, if st.spawning { 0 } else { old }) {
                            // Its models would go with the mod's pack.
                            "a spawned character uses the loaded pack (defeat it first)".into()
                        } else if st.spawning {
                            match start_fetch(m, st, &pname) {
                                Ok(()) => {
                                    st.become_step = Become::Loading { profile: st.profile, waited: 0 };
                                    format!("loading {pname} ...")
                                }
                                Err(e) => e,
                            }
                        } else {
                            st.become_pack = Some(pname.clone());
                            st.become_step = Become::Loading { profile: st.profile, waited: 0 };
                            format!("loading {pname} ...")
                        }
                    }
                    (None, _) => "no profiles loaded".into(),
                    _ => "no player in this scene".into(),
                },
                Entry::Stage => {
                    // What the stage select does on start (0x1046E8 -> 0x10D708): 0x10D680 with
                    // the game's root task; an extra stage through stage 0's name.
                    let extra = st.stage.checked_sub(5);
                    let stage = match extra {
                        Some(e) => {
                            crate::devil_debug::set_stage0(m, Some(e));
                            st.stage_restore = 30;
                            crate::devil_debug::EXTRA_STAGES[e].stage as usize
                        }
                        None => st.stage,
                    };
                    st.noclip = None;
                    restore(m, st);
                    request_thread_call(START_STAGE, &[word(m, ROOT_TASK) as u64, stage as u64]);
                    // A boss room: its warp index replaces the stage's start in the same batch,
                    // before GAME MAIN's state 7 reads it.
                    if let Some(idx) = extra.and_then(|e| crate::devil_debug::extra_warp(m, e)) {
                        crate::devil_debug::request_warp(m, idx);
                    }
                    format!("starting {}", stage_name(m, st.stage))
                }
                Entry::Item => {
                    let (kind, label) = ITEMS[st.item];
                    match spawn_gimmick(m, p, kind) {
                        Ok(()) => {
                            if kind == 0x873 {
                                request_thread_call(GOLD_AMOUNT, &[PREV_RESULT, 100, 500]);
                            }
                            format!("spawned {label}")
                        }
                        Err(e) => e,
                    }
                }
                Entry::Object => match gimmick_types(m).get(st.object) {
                    Some(&kind) => match spawn_gimmick(m, p, kind) {
                        Ok(()) => format!("spawned gimmick {kind:#x}"),
                        Err(e) => e,
                    },
                    None => "no gimmick types loaded".into(),
                },
                Entry::NoClip => match (st.noclip, p) {
                    (Some(_), _) => {
                        st.noclip = None;
                        "no clip off".into()
                    }
                    (None, 0) => "no player in this scene".into(),
                    (None, p) => {
                        st.noclip = Some(position(m, p).map(f32::from_bits));
                        "no clip on: left stick move, R2 / L2 up / down".into()
                    }
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
                    ps2_runtime::pad::BLOCK.store(false, std::sync::atomic::Ordering::Relaxed);
                    String::new()
                }
            };
        }
    }
    let target = chars.get(st.pick).map(|&t| name(m, t)).unwrap_or_else(|| "-".into());
    let prof = profs
        .get(st.profile)
        .map(|p| {
            let note = if char_ready(m, &p.2) {
                ""
            } else if !crate::devil_chars::CHAR_PACKS.iter().any(|c| c.profile == p.2) && pack_of(&model_base(&p.2)).is_some() {
                " [no textures for this variant on the disc]"
            } else if char_packs(&p.2).is_some() {
                " [other area: loads its pack]"
            } else {
                " [no data on the disc]"
            };
            format!("{} ({:#x}){note}", p.2, p.0)
        })
        .unwrap_or_else(|| "-".into());
    let on = |b: bool| if b { "ON" } else { "OFF" };
    out.push_str("MOD MENU (Devil Within, branch devil/debug)\n");
    for (i, e) in ENTRIES.iter().enumerate() {
        out.push_str(if i == st.cur { "> " } else { "  " });
        out.push_str(&match e {
            Entry::Become => format!("Become: < {prof} >  ({}/{})", st.profile + 1, profs.len()),
            Entry::Spawn => format!("Spawn: < {prof} >"),
            Entry::Stage => format!("Stage: < {} >", stage_name(m, st.stage)),
            Entry::Item => format!("Item: < {} >", ITEMS[st.item].1),
            Entry::Object => {
                let types = gimmick_types(m);
                match types.get(st.object) {
                    Some(k) => format!("Object: < gimmick type {k:#x} >  ({}/{}, may need its room)", st.object + 1, types.len()),
                    None => "Object: -".into(),
                }
            }
            Entry::NoClip => format!("No clip (character, camera follows): {}", on(st.noclip.is_some())),
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
    out.push_str("--\nUp/Down: entry   Left/Right: choose   CIRCLE (C): run   L3+R3: close (the game gets no input while open)\n\n");
}
