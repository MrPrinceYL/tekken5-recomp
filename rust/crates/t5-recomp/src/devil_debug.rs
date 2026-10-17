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

use ps2_runtime::bus::{ram_read, ram_write};
use ps2_runtime::Machine;

const MODE_TASK: u32 = 0x002A_AAF4;
const TITLE_TASK_FN: u32 = 0x0010_3288;
const DEV_MENU_ENTER: u32 = 0x0010_34A0;

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
        return;
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
