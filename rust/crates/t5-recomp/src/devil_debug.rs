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
//! The retail build removed the menus' text output (the developer menu's loop over its entries
//! is empty, the SUBMENU draw callback 0x132618 is `jr ra`), so they would run invisibly:
//! `menu_text` reads the open menus from guest memory every vertical blank for the front end's
//! overlay (`MENU_TEXT`), in place of the removed output.
//!
//! Both act once per start of the program, at the moment the boot task (pointer at 0x2AAAF4)
//! switches from waiting (0x103438) to the title (0x103288, state 0): the title waits three
//! frames before it reads its menu number (+0x54).

use ps2_runtime::bus::{ram_read, ram_write};
use ps2_runtime::Machine;

const MODE_TASK: u32 = 0x002A_AAF4;
const TITLE_TASK_FN: u32 = 0x0010_3288;
const DEV_MENU_ENTER: u32 = 0x0010_34A0;
/// The task pool (0x20AEA0): +8 first task; tasks are chained through +0x18.
const TASK_POOL: u32 = 0x002A_9A30;
/// Developer menu task function; table (function, name) at +0x38 up to DEV_MENU_END, cursor
/// +0x40, open while +0x34 == 1.
const DEV_MENU_FN: u32 = 0x0010_3038;
const DEV_MENU_END: u32 = 0x0020_CF90;
/// SUBMENU task function (set up by 0x132430): table +0x38 up to a 0 function, title string
/// +0x3C, cursor +0x40, open while +0x34 == 0.
const SUBMENU_FN: u32 = 0x0013_24B8;

/// The open debug menus as text (empty: none), for the front end.
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
    let text = menu_text(m);
    if let Ok(mut t) = MENU_TEXT.lock() {
        if *t != text {
            *t = text;
        }
    }
    if DONE.load(Relaxed) {
        return;
    }
    let task = ram_read(m, MODE_TASK, 4) as u32;
    if task == 0 || ram_read(m, task + 0x20, 4) as u32 != TITLE_TASK_FN || ram_read(m, task + 0x34, 4) != 0 {
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

fn cstr(m: &Machine, a: u32) -> String {
    (0..64).map(|i| ram_read(m, a + i, 1) as u8).take_while(|&b| b != 0).map(|b| b as char).collect()
}

/// Controls, from the menus' input code (pad words at 0x2A98E0..: libpad bits, 0x10 up, 0x40
/// down, 0x2000 circle, 0x4000 cross; keys = t5-bevy's default bindings). The developer menu
/// (0x103038) selects with circle only, SUBMENU (0x1324B8) with circle or cross; neither has a
/// "back": the developer menu waits in state 2 until the started mode's task ends (0x103038),
/// then shows itself again.
const DEV_HINT: &str = "Steuerkreuz hoch/runter (Pfeiltasten): Eintrag waehlen\nKREIS (Taste C): starten\nZurueck: endet ein Modus, kommt dieses Menue wieder";
const SUB_HINT: &str = "Steuerkreuz hoch/runter (Pfeiltasten): Eintrag waehlen\nKREIS (Taste C) oder KREUZ (Taste X): starten";

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
        out.push_str(&format!("{mark}{}\n", cstr(m, word(m, e + 4))));
    }
    out.push_str("--\n");
    out.push_str(hint);
    out.push_str("\n\n");
}

fn menu_text(m: &Machine) -> String {
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
        }
        t = word(m, t + 0x18);
    }
    out
}
