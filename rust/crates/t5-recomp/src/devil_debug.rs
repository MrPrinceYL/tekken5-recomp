//! Devil Within's developer menus, left in the retail program but unreachable (DevilWithin.md).
//! Opt-in with T5_DW_DEBUG; unset, nothing changes. Only guest data is set, the way the game's
//! own debug paths do it; the translated code is the game's.
//!
//! - `title`: the debug title menu "TITLE MENU (DUMMY)" (TITLE_MENU state 5, what the developer
//!   menu entry "TITLE(Debug)" 0x1038B0 selects): NEW GAME, NEW GAME(DEVIL), CONTINUE,
//!   LOAD GAME, STAGE SELECT, SELECT DEVIL, EXTRA-DEBUG.
//! - `dev`: the developer mode menu 0x103038 (table 0x2AD610: MAKINO MENU, SHUJI TEST, ...,
//!   CHARA VIEW, MODEL VIEW, STREAM TEST, GAME LOAD START, TITLE(Debug), TITLE(Release), ...),
//!   entered through 0x1034A0, which nothing in the retail program calls (its trigger 0x103670
//!   is an empty stub).
//!
//! Both act once per start of the program, at the moment the boot task (pointer at 0x2AAAF4)
//! switches from waiting (0x103438) to the title (0x103288, state 0): the title waits three
//! frames before it reads its menu number (+0x54).
//!
//! The retail build removed the debug text output (the developer menu's loop over its entries
//! is empty, the SUBMENU draw callback 0x132618 is `jr ra`, the tools' draw callbacks print
//! nothing), so menus and tools would run invisibly. `overlay` reads the open menus and the
//! running tools from guest memory every vertical blank for the front end (`MENU_TEXT`): the
//! entries, the cursor, the tools' current values and their controls (read from each tool's
//! input code, DevilWithin.md), in place of the removed output. The overlay text is English.
//!
//! One repair, also only with T5_DW_DEBUG: an entry whose code the retail build emptied
//! (`jr ra`) returns at once; the developer menu then shows itself again with its entry count
//! cleared by 0x102FD8 (only its state 0 counts, and the re-entry 0x1034A0 that would run it is
//! unused), and the next up / down loops forever over the -1st entry (0x1031E8). Setting state 0
//! re-counts, as entering through 0x1034A0 does.

use ps2_runtime::bus::{ram_read, ram_write};
use ps2_runtime::Machine;

const MODE_TASK: u32 = 0x002A_AAF4;
const TITLE_TASK_FN: u32 = 0x0010_3288;
const DEV_MENU_ENTER: u32 = 0x0010_34A0;
/// The task pool (0x20AEA0): +8 first task; tasks are chained through +0x18.
const TASK_POOL: u32 = 0x002A_9A30;
/// Developer menu task function; table (function, name) at +0x38 up to DEV_MENU_END, cursor
/// +0x40, entry count +0x3C, open while +0x34 == 1.
const DEV_MENU_FN: u32 = 0x0010_3038;
const DEV_MENU_END: u32 = 0x0020_CF90;
/// SUBMENU task function (set up by 0x132430): table +0x38 up to a 0 function, title string
/// +0x3C, cursor +0x40, open while +0x34 == 0.
const SUBMENU_FN: u32 = 0x0013_24B8;

/// Entries that start a task with no working code: the developer menu waits for it forever.
const HANGS: &[u32] = &[0x001F_7F88 /* HASHI TEST: every callback is `jr ra` */];

/// The open debug menus and tools as text (empty: none), for the front end.
pub static MENU_TEXT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Title,
    Dev,
}

fn mode() -> Option<Mode> {
    static M: std::sync::OnceLock<Option<Mode>> = std::sync::OnceLock::new();
    *M.get_or_init(|| match std::env::var("T5_DW_DEBUG").ok()?.as_str() {
        "title" | "1" => Some(Mode::Title),
        "dev" => Some(Mode::Dev),
        _ => None,
    })
}

/// Install the hook when T5_DW_DEBUG asks for it.
pub fn install() {
    if mode().is_some() {
        let _ = ps2_runtime::VBLANK_HOOK.set(hook);
    }
}

fn hook(m: &mut Machine) {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static DONE: AtomicBool = AtomicBool::new(false);
    // Devil Within is entered by a jump of the program loader, not ExecPS2: the current program
    // stays "loader", so look for its image in RAM (as the dispatcher does).
    let running = m.programs.iter().find(|p| p.name == "devil").is_some_and(|p| m.in_ram(p));
    if !running {
        DONE.store(false, Relaxed);
        if let Ok(mut t) = MENU_TEXT.lock() {
            t.clear();
        }
        return;
    }
    let mut text = overlay(m);
    cheat_menu(m, &mut text);
    if let Ok(mut t) = MENU_TEXT.lock() {
        if *t != text {
            *t = text;
        }
    }
    let task = word(m, MODE_TASK);
    if task != 0 && word(m, task + 0x20) == DEV_MENU_FN && word(m, task + 0x34) == 1 && word(m, task + 0x3C) == 0 {
        ram_write(m, task + 0x34, 4, 0);
    }
    if DONE.load(Relaxed) {
        return;
    }
    if task == 0 || word(m, task + 0x20) != TITLE_TASK_FN || word(m, task + 0x34) != 0 {
        return;
    }
    DONE.store(true, Relaxed);
    match mode() {
        Some(Mode::Title) => {
            ram_write(m, task + 0x54, 4, 5);
            ps2_runtime::diag::log("devil: T5_DW_DEBUG=title -> TITLE MENU (DUMMY)".into());
        }
        Some(Mode::Dev) => {
            ram_write(m, task + 0x20, 4, DEV_MENU_ENTER as u64);
            ps2_runtime::diag::log("devil: T5_DW_DEBUG=dev -> developer mode menu".into());
        }
        None => {}
    }
}

fn word(m: &Machine, a: u32) -> u32 {
    ram_read(m, a, 4) as u32
}

fn int(m: &Machine, a: u32) -> i32 {
    word(m, a) as i32
}

/// A guest string; Shift-JIS text (not in the overlay font) by its known translation.
fn cstr(m: &Machine, a: u32) -> String {
    let b: Vec<u8> = (0..64).map(|i| ram_read(m, a + i, 1) as u8).take_while(|&b| b != 0).collect();
    if b.is_ascii() {
        return b.iter().map(|&c| c as char).collect();
    }
    match b.as_slice() {
        [0x8A, 0x4A, 0x94, 0xAD, 0x83, 0x82, 0x81, 0x5B, 0x83, 0x68] => "KAIHATSU MODE (developer mode)".into(),
        [0x91, 0xCC, 0x97, 0xCD, 0x33, 0x30, 0x81, 0x93, 0x89, 0xF1, 0x95, 0x9C] => "Restore 30% health".into(),
        _ => "(Japanese text)".into(),
    }
}

/// Whether the retail build emptied the function (`jr ra; nop`).
fn emptied(m: &Machine, f: u32) -> bool {
    word(m, f) == 0x03E0_0008 && word(m, f + 4) == 0
}

// Controls: from each menu's / tool's input code (pad words at 0x2A98E0 held, +4 pressed,
// +8 repeat; libpad bits: 0x10 up, 0x20 right, 0x40 down, 0x80 left, 0x100 L2, 0x200 R2,
// 0x400 L1, 0x800 R1, 0x1000 triangle, 0x2000 circle, 0x4000 cross, 0x8000 square, 8 start,
// 1 select, 4 R3; analog sticks at 0x2A98A0). Keys: t5-bevy's default keyboard bindings.
const DEV_HINT: &str = "Up/Down (arrow keys): entry   CIRCLE (C): start\n\
     [removed] = code deleted in the retail build, starts nothing\n\
     [hangs] = starts an empty mode, only a restart helps\n\
     No back button: when a mode ends, this menu returns";
const SUB_HINT: &str = "Up/Down (arrow keys): entry   CIRCLE (C) or CROSS (X): start";

/// One menu: title line, then its entries (null entries skipped by the game too), the cursor's
/// entry marked, then the controls.
fn menu(m: &Machine, title: &str, table: u32, cursor: u32, end: u32, hint: &str, out: &mut String) {
    out.push_str(title);
    out.push('\n');
    for i in 0..64 {
        let e = table + i * 8;
        let f = word(m, e);
        if f == end {
            break;
        }
        if f == 0 {
            continue;
        }
        let mark = if i == cursor { "> " } else { "  " };
        let note = if emptied(m, f) {
            "  [removed]"
        } else if HANGS.contains(&f) {
            "  [hangs]"
        } else {
            ""
        };
        out.push_str(&format!("{mark}{}{note}\n", cstr(m, word(m, e + 4))));
    }
    out.push_str("--\n");
    out.push_str(hint);
    out.push_str("\n\n");
}

/// A running tool: its name, current values and controls.
fn tool(m: &Machine, t: u32, f: u32) -> Option<String> {
    let s = |o: u32| int(m, t + o);
    Some(match f {
        // MODEL VIEW 0x108F10, update 0x1092B8: state +0x34 0 = model list (0x1091B0), 1 = view.
        0x0010_92B8 if s(0x34) == 0 => format!(
            "MODEL VIEW - model list\nEntry {} (model id {})\n--\n\
             Up/Down: model   CIRCLE (C) or START (Enter): view   SQUARE (Z): background",
            s(0x148),
            s(0x3C)
        ),
        0x0010_92B8 => format!(
            "MODEL VIEW - view (model id {})\n--\n\
             Left stick: move   Right stick: rotate\n\
             L1 / R1 (held): closer / farther   hold CIRCLE: faster\n\
             L2 / R2: value -/+ (0x2B849C)   TRIANGLE (V): toggle display\n\
             SQUARE (Z): background   START (Enter): reset camera\n\
             CROSS (X) or SELECT (Tab): back to the list",
            s(0x3C)
        ),
        // SOUND TEST 0x133BC0, update 0x133CE8.
        0x0013_3CE8 => format!(
            "SOUND TEST\nBank {} (0-20)   Sound {}\n--\n\
             Left/Right: bank   Up/Down: sound\n\
             hold CIRCLE (C): play   CROSS (X): stop sound\n\
             SQUARE (Z): load bank   TRIANGLE (V): all voices off",
            s(0x38),
            s(0x3C)
        ),
        // STREAM TEST 0x1344F8 (EVENT_STREAM_TEST), update 0x1346A0.
        0x0013_46A0 => format!(
            "STREAM TEST\nStream {} of {}   Volume {:.2}\n--\n\
             Up/Down: stream (hold CIRCLE: steps of 10)   Left/Right: volume\n\
             CIRCLE / TRIANGLE / SQUARE: play (variant 0 / 1 / 2)\n\
             CROSS (X): stop   SELECT (Tab): stop (at once)   START (Enter): pause on/off",
            s(0x3C),
            s(0x38),
            f32::from_bits(word(m, t + 0x4C))
        ),
        // TEXTURE VIEWER 0x109A70, update 0x109AF0 / 0x109B78.
        0x0010_9AF0 => format!(
            "TEXTURE VIEWER\nPosition {:#x}   View {}   Memory {}\n--\n\
             Left/Right: browse (hold TRIANGLE: x256, hold CIRCLE: x16)\n\
             CROSS (X): view (0-3)   R1: memory area (0-7)",
            s(0x34),
            word(m, 0x0041_AF6C),
            word(m, 0x0041_AF70)
        ),
        // OPTION TEST 0x105E30, update 0x105E98: records the directions and cross.
        0x0010_5E98 => "OPTION TEST (input test)\n--\n\
             D-pad and CROSS (X) are recorded (display removed in the retail build)"
            .to_string(),
        // MAP VIEWER 0x108D60, update 0x108DC8: KREIS loads the map if it exists (0x125F60) into
        // EVENT_SAMPLE_MODE.
        0x0010_8DC8 => map_viewer(m, s(0x34)),
        // EVENT_SAMPLE_MODE 0x106728 (map loaded from MAP VIEWER / SOUND 3D TEST), 0x107F30 /
        // 0x108120: free camera.
        0x0010_7F30 => "MAP (EVENT_SAMPLE_MODE, free camera)\n--\n\
             Left stick: move   Right stick: turn   R2 / L2: up / down\n\
             hold CIRCLE: faster   hold SQUARE: special view   R3: reset"
            .to_string(),
        // CHAR PROFILE LIST 0x120478, update 0x1204B8.
        0x0012_04B8 => format!("CHAR PROFILE LIST\nRow {}\n--\nUp/Down: scroll", s(0x34)),
        // STAGE SELECT / SELECT DEVIL / EXTRA-DEBUG 0x103DC8 (+0x40 mode 1 / 5 / 7), update
        // 0x103E88: state 1 stage +0x3C (0-4, 5 = back), state 2 value +0x38 (0-3, 0x103B00,
        // stored by 0x105BD0 in 0x2AD58C).
        0x0010_3E88 => {
            let what = match s(0x40) {
                5 => "SELECT DEVIL",
                7 => "EXTRA-DEBUG",
                _ => "STAGE SELECT",
            };
            if s(0x34) == 2 {
                format!(
                    "{what} - stage {}\nLevel {} (0-3)\n--\n\
                     Up/Down: level   CIRCLE (C): start (controller 1 or 2)   CROSS (X): back",
                    s(0x3C),
                    s(0x38)
                )
            } else {
                let st = s(0x3C);
                let name = if st == 5 { "back".to_string() } else { st.to_string() };
                format!("{what}\nStage {name} (0-4, 5 = back)\n--\nUp/Down: stage   CIRCLE (C): next")
            }
        }
        // NEW GAME / NEW GAME(DEVIL) 0x103B70 / 0x103BC0 (NEWGAME START), update 0x103C10.
        0x0010_3C10 => format!(
            "NEW GAME\nLevel {} (0-3)\n--\nUp/Down: level   CIRCLE (C): start (controller 1 or 2)",
            s(0x38)
        ),
        _ => return None,
    })
}

/// The developer warp list `area/mpl.txt` as parsed by 0x125D38: count at 0x41B55C, table at
/// *0x41B558, 0x50 bytes per entry (+0 area, +0xE number, +0x10 point, +0x30 label in
/// Shift-JIS).
const MPL_TABLE: u32 = 0x0041_B558;
const MPL_COUNT: u32 = 0x0041_B55C;
/// Areas with complete files in TK5DATA2 (the program's directory 0x362900, packs checked
/// against their lists, DevilWithin.md). `chr` has lists and scripts but not its room geometry;
/// the other entries (bgtest, effect, kondoh, matty, mine, szk) name test maps with no files.
const AREAS_ON_DISC: &[&str] = &["lab_A", "lab_B", "egy_A", "blu_A", "blu_B", "makino"];

/// English for the Japanese words of the warp list labels (Shift-JIS, longest first).
const MPL_WORDS: &[(&[u8], &str)] = &[
    (b"\x83\x47\x83\x74\x83\x46\x83\x4e\x83\x67\x8c\x9f\x8f\xd8\x97\x70\x83\x7d\x83\x62\x83\x76\x83\x66\x81\x5b\x83\x5e", "effect check map"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x65\x83\x58\x83\x67\x81\x40\x83\x82\x83\x93\x83\x58\x83\x5e\x81\x5b", "character test monster"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x65\x83\x58\x83\x67\x81\x40\x83\x66\x83\x55\x81\x5b\x83\x67", "character test desert"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x65\x83\x58\x83\x67\x81\x40\x93\x53\x8c\x9d", "character test Tekken"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x4e\x83\x5e\x81\x5b\x8c\x9f\x8f\xd8\x97\x70", "character check room"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x4e\x83\x5e\x81\x5b\x83\x65\x83\x58\x83\x67", "character test"),
    (b"\x91\x93\x82\xa2\x88\xe2\x90\xd5\x83\x54\x83\x93\x83\x76\x83\x8b", "sample Blue Ruins "),
    (b"\x83\x47\x83\x57\x83\x76\x83\x67\x83\x54\x83\x93\x83\x76\x83\x8b", "sample Egypt "),
    (b"\x8c\xa4\x8b\x86\x8e\xba\x83\x54\x83\x93\x83\x76\x83\x8b", "sample Laboratory "),
    (b"\x97\xd1\x83\x54\x83\x93\x83\x76\x83\x8b", "sample FOREST (no such area in the game) "),
    (b"\x97\x86\x90\xf9\x8a\x4b\x92\x69\x83\x65\x83\x58\x83\x67", "spiral staircase test "),
    (b"\x81\x40\x83\x5a\x81\x5b\x83\x75\x95\x94\x89\xae", " save room"),
    (b"\x96\x71\x96\xec\x83\x65\x83\x58\x83\x67", "Makino test "),
    (b"\x95\x89\x89\xd7\x83\x65\x83\x58\x83\x67", "load test "),
    (b"\x81\x40\x91\x93\x82\xa2\x88\xe2\x90\xd5", " Blue Ruins "),
    (b"\x8e\x52\x93\xe0\x83\x65\x83\x58\x83\x67", "Yamauchi test"),
    (b"\x95\x89\x89\xd7\x8c\x9f\x8f\xd8\x97\x70", "load check"),
    (b"\x83\x65\x83\x58\x83\x67\x95\x94\x89\xae", " test room "),
    (b"\x81\x40\x83\x47\x83\x57\x83\x76\x83\x67", " Egypt "),
    (b"\x81\x40\x8c\xa4\x8b\x86\x8e\xba", " Laboratory "),
    (b"\x81\x40\x83\x7b\x83\x58", " BOSS"),
    (b"\x97\x86\x90\xf9", " spiral"),
    (b"\x8e\x52\x93\xe0", "Yamauchi"),
    (b"\x83\x65\x83\x58\x83\x67", "test"),
    (b"\x81\x69", "("),
    (b"\x81\x6a", ")"),
    (b"\x81\xa5", ""),
    (b"\x81\x40", " "),
    (b"\x82\x50", "1"),
    (b"\x82\x51", "2"),
];

fn mpl_label(m: &Machine, a: u32) -> String {
    let b: Vec<u8> = (0..0x20).map(|i| ram_read(m, a + i, 1) as u8).take_while(|&b| b != 0).collect();
    let mut out = String::new();
    let mut i = 0;
    'next: while i < b.len() {
        if b[i] < 0x80 {
            out.push(b[i] as char);
            i += 1;
            continue;
        }
        for (jp, en) in MPL_WORDS {
            if b[i..].starts_with(jp) {
                out.push_str(en);
                i += jp.len();
                continue 'next;
            }
        }
        out.push('?');
        i += 2;
    }
    out
}

/// MAP VIEWER: the warp list around the cursor, labels in English, test maps without data
/// marked.
fn map_viewer(m: &Machine, cur: i32) -> String {
    let table = word(m, MPL_TABLE);
    let count = int(m, MPL_COUNT);
    let mut out = format!("MAP VIEWER (warp list area/mpl.txt, {count} entries)\n");
    if table == 0 || count <= 0 {
        out.push_str("List not loaded\n");
    }
    for i in (cur - 6).max(0)..(cur + 7).min(count) {
        let e = table + i as u32 * 0x50;
        let area = cstr(m, e);
        let mark = if i == cur { "> " } else { "  " };
        let missing = match area.as_str() {
            a if AREAS_ON_DISC.contains(&a) => "",
            "chr" => "  [no room geometry]",
            _ => "  [no data]",
        };
        out.push_str(&format!(
            "{mark}{i:2} {area} {} - {}{missing}\n",
            ram_read(m, e + 0xE, 2) as u16,
            mpl_label(m, e + 0x30).trim()
        ));
    }
    out.push_str(
        "--\nUp/Down: map   CIRCLE (C): load\n\
         [no data] = test map without files on the disc, do not load\n\
         [no room geometry] = room models missing on the disc",
    );
    out
}

/// The "matty debug" tree (MATTY TEST, nodes from 0x356130, `mt_ndebug_util_list.c`): set up by
/// 0x1CBFF8, but its driver (input / draw 0x1D8260, 0x1D88B0) is unreferenced in the retail
/// build. Its entries as an in-game menu: (name string in the program, function, why not).
/// Functions without arguments run through `kernel::request_guest_call`; the others need the
/// removed tree view's context (auto-attack reads its node, the lists build tree views).
const CHEATS: &[(u32, u32, &str)] = &[
    (0x0040_EE38, 0x001D_92B0, ""),                                  // 30% health: life += max * k
    (0x0040_EED8, 0x001D_9A78, ""),                                  // flags 0x200.. (5 x 8) = 1
    (0x0040_EEF0, 0x001D_9AF0, ""),                                  // the same = 0
    (0x0040_EE20, 0, "removed in the retail build"),                 // its function 0x1D93B8 is empty
    (0x0040_EEB8, 0, "needs the removed debug tree view"),           // 0x1D9970(node)
    (0x0040_EEC8, 0, "needs the removed debug tree view"),           // value node
    (0x0040_EE58, 0, "needs the removed debug tree view"),           // 0x1D96B8 list view
    (0x0040_EE70, 0, "needs the removed debug tree view"),
    (0x0040_EE88, 0, "needs the removed debug tree view"),
];

/// Pad words of port 1 (0x2A9890 + 0x50: held).
const PAD_HELD: u32 = 0x002A_98E0;
const SELECT: u32 = 0x1;
const UP: u32 = 0x10;
const DOWN: u32 = 0x40;
const L1: u32 = 0x400;
const CIRCLE: u32 = 0x2000;

/// In-game cheat menu: SELECT held + L1 opens / closes it, SELECT held + Up / Down chooses,
/// SELECT held + CIRCLE runs the entry. Edges from the held word between vertical blanks.
fn cheat_menu(m: &Machine, out: &mut String) {
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
    static OPEN: AtomicBool = AtomicBool::new(false);
    static CUR: AtomicUsize = AtomicUsize::new(0);
    static PREV: AtomicU32 = AtomicU32::new(0);
    static LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    let held = word(m, PAD_HELD);
    let new = held & !PREV.swap(held, Relaxed);
    if held & SELECT != 0 && new & L1 != 0 {
        OPEN.fetch_xor(true, Relaxed);
    }
    if !OPEN.load(Relaxed) {
        return;
    }
    let mut cur = CUR.load(Relaxed);
    if held & SELECT != 0 {
        if new & UP != 0 {
            cur = (cur + CHEATS.len() - 1) % CHEATS.len();
        }
        if new & DOWN != 0 {
            cur = (cur + 1) % CHEATS.len();
        }
        if new & CIRCLE != 0 {
            let (name, f, why) = CHEATS[cur];
            let msg = if f != 0 {
                ps2_runtime::kernel::request_guest_call(f);
                ps2_runtime::diag::log(format!("devil: cheat menu runs {f:#x}"));
                format!("ran: {}", cstr(m, name))
            } else {
                format!("not available: {why}")
            };
            if let Ok(mut l) = LAST.lock() {
                *l = msg;
            }
        }
    }
    CUR.store(cur, Relaxed);
    out.push_str("CHEAT MENU (matty debug)\n");
    for (i, (name, f, _)) in CHEATS.iter().enumerate() {
        let mark = if i == cur { "> " } else { "  " };
        let note = if *f == 0 { "  [n/a]" } else { "" };
        out.push_str(&format!("{mark}{}{note}\n", cstr(m, *name)));
    }
    if let Ok(l) = LAST.lock() {
        if !l.is_empty() {
            out.push_str(&format!("{l}\n"));
        }
    }
    out.push_str("--\nhold SELECT (Tab) + Up/Down: entry   + CIRCLE (C): run   + L1: close\n\n");
}

fn overlay(m: &Machine) -> String {
    let mut out = String::new();
    let mut t = word(m, TASK_POOL + 8);
    for _ in 0..1024 {
        if t == 0 {
            break;
        }
        let f = word(m, t + 0x20);
        let state = word(m, t + 0x34);
        if f == DEV_MENU_FN && state == 1 {
            menu(m, "DEVELOPER MENU", word(m, t + 0x38), word(m, t + 0x40), DEV_MENU_END, DEV_HINT, &mut out);
        } else if f == SUBMENU_FN && state == 0 {
            let title = match word(m, t + 0x3C) {
                0 => "SUBMENU".to_string(),
                a => cstr(m, a),
            };
            menu(m, &title, word(m, t + 0x38), word(m, t + 0x40), 0, SUB_HINT, &mut out);
        } else if let Some(text) = tool(m, t, f) {
            out.push_str(&text);
            out.push_str("\n\n");
        }
        t = word(m, t + 0x18);
    }
    out
}
