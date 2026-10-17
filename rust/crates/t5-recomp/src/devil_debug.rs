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
//! input code, DevilWithin.md), in place of the removed output.
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
    let text = overlay(m);
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
        [0x8A, 0x4A, 0x94, 0xAD, 0x83, 0x82, 0x81, 0x5B, 0x83, 0x68] => "KAIHATSU MODE (Entwicklermodus)".into(),
        _ => "(japanischer Text)".into(),
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
const DEV_HINT: &str = "Hoch/Runter (Pfeiltasten): Eintrag   KREIS (C): starten\n\
     [entfernt] = Code im Release geloescht, startet nichts\n\
     [haengt] = startet einen leeren Modus, nur Neustart hilft\n\
     Kein Zurueck: endet ein Modus, kommt dieses Menue wieder";
const SUB_HINT: &str = "Hoch/Runter (Pfeiltasten): Eintrag   KREIS (C) oder KREUZ (X): starten";

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
            "  [entfernt]"
        } else if HANGS.contains(&f) {
            "  [haengt]"
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
            "MODEL VIEW - Modellliste\nEintrag {} (Modell-ID {})\n--\n\
             Hoch/Runter: Modell   KREIS (C) oder START (Enter): anzeigen   VIERECK (Z): Hintergrund",
            s(0x148),
            s(0x3C)
        ),
        0x0010_92B8 => format!(
            "MODEL VIEW - Ansicht (Modell-ID {})\n--\n\
             Linker Stick: verschieben   Rechter Stick: drehen\n\
             L1 / R1 (gehalten): naeher / weiter   KREIS halten: schneller\n\
             L2 / R2: Wert -/+ (0x2B849C)   DREIECK (V): Anzeige umschalten\n\
             VIERECK (Z): Hintergrund   START (Enter): Kamera zuruecksetzen\n\
             KREUZ (X) oder SELECT (Tab): zurueck zur Liste",
            s(0x3C)
        ),
        // SOUND TEST 0x133BC0, update 0x133CE8.
        0x0013_3CE8 => format!(
            "SOUND TEST\nBank {} (0-20)   Ton {}\n--\n\
             Links/Rechts: Bank   Hoch/Runter: Ton\n\
             KREIS (C) halten: Ton spielen   KREUZ (X): Ton stoppen\n\
             VIERECK (Z): Bank laden   DREIECK (V): alle Stimmen aus",
            s(0x38),
            s(0x3C)
        ),
        // STREAM TEST 0x1344F8 (EVENT_STREAM_TEST), update 0x1346A0.
        0x0013_46A0 => format!(
            "STREAM TEST\nStream {} von {}   Lautstaerke {:.2}\n--\n\
             Hoch/Runter: Stream (KREIS halten: 10er-Schritte)   Links/Rechts: Lautstaerke\n\
             KREIS / DREIECK / VIERECK: abspielen (Variante 0 / 1 / 2)\n\
             KREUZ (X): stoppen   SELECT (Tab): stoppen (sofort)   START (Enter): Pause an/aus",
            s(0x3C),
            s(0x38),
            f32::from_bits(word(m, t + 0x4C))
        ),
        // TEXTURE VIEWER 0x109A70, update 0x109AF0 / 0x109B78.
        0x0010_9AF0 => format!(
            "TEXTURE VIEWER\nPosition {:#x}   Ansicht {}   Speicher {}\n--\n\
             Links/Rechts: blaettern (DREIECK halten: x256, KREIS halten: x16)\n\
             KREUZ (X): Ansicht (0-3)   R1: Speicherbereich (0-7)",
            s(0x34),
            word(m, 0x0041_AF6C),
            word(m, 0x0041_AF70)
        ),
        // OPTION TEST 0x105E30, update 0x105E98: records the directions and cross.
        0x0010_5E98 => "OPTION TEST (Eingabetest)\n--\n\
             Steuerkreuz und KREUZ (X) werden registriert (Anzeige im Release entfernt)"
            .to_string(),
        // MAP VIEWER 0x108D60, update 0x108DC8: KREIS loads the map if it exists (0x125F60) into
        // EVENT_SAMPLE_MODE.
        0x0010_8DC8 => map_viewer(m, s(0x34)),
        // EVENT_SAMPLE_MODE 0x106728 (map loaded from MAP VIEWER / SOUND 3D TEST), 0x107F30 /
        // 0x108120: free camera.
        0x0010_7F30 => "KARTE (EVENT_SAMPLE_MODE, freie Kamera)\n--\n\
             Linker Stick: bewegen   Rechter Stick: drehen   R2 / L2: hoch / runter\n\
             KREIS halten: schneller   VIERECK halten: Spezialansicht   R3: zuruecksetzen"
            .to_string(),
        // CHAR PROFILE LIST 0x120478, update 0x1204B8.
        0x0012_04B8 => format!("CHAR PROFILE LIST\nZeile {}\n--\nHoch/Runter: blaettern", s(0x34)),
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
                    "{what} - Stage {}\nStufe {} (0-3)\n--\n\
                     Hoch/Runter: Stufe   KREIS (C): starten (Controller 1 oder 2)   KREUZ (X): zurueck",
                    s(0x3C),
                    s(0x38)
                )
            } else {
                let st = s(0x3C);
                let name = if st == 5 { "zurueck".to_string() } else { st.to_string() };
                format!("{what}\nStage {name} (0-4, 5 = zurueck)\n--\nHoch/Runter: Stage   KREIS (C): weiter")
            }
        }
        // NEW GAME / NEW GAME(DEVIL) 0x103B70 / 0x103BC0 (NEWGAME START), update 0x103C10.
        0x0010_3C10 => format!(
            "NEW GAME\nStufe {} (0-3)\n--\nHoch/Runter: Stufe   KREIS (C): starten (Controller 1 oder 2)",
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
/// Areas with files in TK5DATA2 (the program's directory 0x362900); the other entries of the
/// list (bgtest, effect, kondoh, matty, mine, szk) name test maps whose data is not on the disc.
const AREAS_ON_DISC: &[&str] = &["lab_A", "lab_B", "egy_A", "blu_A", "blu_B", "chr", "makino"];

/// German for the Japanese words of the warp list labels (Shift-JIS, longest first).
const MPL_WORDS: &[(&[u8], &str)] = &[
    (b"\x83\x47\x83\x74\x83\x46\x83\x4e\x83\x67\x8c\x9f\x8f\xd8\x97\x70\x83\x7d\x83\x62\x83\x76\x83\x66\x81\x5b\x83\x5e", "Effekt-Pruefkarte"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x65\x83\x58\x83\x67\x81\x40\x83\x82\x83\x93\x83\x58\x83\x5e\x81\x5b", "Figurentest Monster"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x65\x83\x58\x83\x67\x81\x40\x83\x66\x83\x55\x81\x5b\x83\x67", "Figurentest Desert"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x65\x83\x58\x83\x67\x81\x40\x93\x53\x8c\x9d", "Figurentest Tekken"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x4e\x83\x5e\x81\x5b\x8c\x9f\x8f\xd8\x97\x70", "Figuren-Pruefraum"),
    (b"\x83\x4c\x83\x83\x83\x89\x83\x4e\x83\x5e\x81\x5b\x83\x65\x83\x58\x83\x67", "Figurentest"),
    (b"\x91\x93\x82\xa2\x88\xe2\x90\xd5\x83\x54\x83\x93\x83\x76\x83\x8b", "Muster Blaue Ruinen "),
    (b"\x83\x47\x83\x57\x83\x76\x83\x67\x83\x54\x83\x93\x83\x76\x83\x8b", "Muster Aegypten "),
    (b"\x8c\xa4\x8b\x86\x8e\xba\x83\x54\x83\x93\x83\x76\x83\x8b", "Muster Labor "),
    (b"\x97\xd1\x83\x54\x83\x93\x83\x76\x83\x8b", "Muster WALD (kein Gebiet im Spiel) "),
    (b"\x97\x86\x90\xf9\x8a\x4b\x92\x69\x83\x65\x83\x58\x83\x67", "Wendeltreppen-Test "),
    (b"\x81\x40\x83\x5a\x81\x5b\x83\x75\x95\x94\x89\xae", " Speicherraum"),
    (b"\x96\x71\x96\xec\x83\x65\x83\x58\x83\x67", "Makino-Test "),
    (b"\x95\x89\x89\xd7\x83\x65\x83\x58\x83\x67", "Lasttest "),
    (b"\x81\x40\x91\x93\x82\xa2\x88\xe2\x90\xd5", " Blaue Ruinen "),
    (b"\x8e\x52\x93\xe0\x83\x65\x83\x58\x83\x67", "Yamauchi-Test"),
    (b"\x95\x89\x89\xd7\x8c\x9f\x8f\xd8\x97\x70", "Lastpruefung"),
    (b"\x83\x65\x83\x58\x83\x67\x95\x94\x89\xae", "Testraum "),
    (b"\x81\x40\x83\x47\x83\x57\x83\x76\x83\x67", " Aegypten "),
    (b"\x81\x40\x8c\xa4\x8b\x86\x8e\xba", " Labor "),
    (b"\x81\x40\x83\x7b\x83\x58", " BOSS"),
    (b"\x97\x86\x90\xf9", " Spirale"),
    (b"\x8e\x52\x93\xe0", "Yamauchi"),
    (b"\x83\x65\x83\x58\x83\x67", "Test"),
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
        for (jp, de) in MPL_WORDS {
            if b[i..].starts_with(jp) {
                out.push_str(de);
                i += jp.len();
                continue 'next;
            }
        }
        out.push('?');
        i += 2;
    }
    out
}

/// MAP VIEWER: the warp list around the cursor, with the labels in German and the test maps
/// whose data is missing marked.
fn map_viewer(m: &Machine, cur: i32) -> String {
    let table = word(m, MPL_TABLE);
    let count = int(m, MPL_COUNT);
    let mut out = format!("MAP VIEWER (Warp-Liste area/mpl.txt, {count} Eintraege)\n");
    if table == 0 || count <= 0 {
        out.push_str("Liste nicht geladen\n");
    }
    for i in (cur - 6).max(0)..(cur + 7).min(count) {
        let e = table + i as u32 * 0x50;
        let area = cstr(m, e);
        let mark = if i == cur { "> " } else { "  " };
        let missing = if AREAS_ON_DISC.contains(&area.as_str()) { "" } else { "  [Daten fehlen]" };
        out.push_str(&format!(
            "{mark}{i:2} {area} {} - {}{missing}\n",
            ram_read(m, e + 0xE, 2) as u16,
            mpl_label(m, e + 0x30).trim()
        ));
    }
    out.push_str("--\nHoch/Runter: Karte   KREIS (C): laden\n[Daten fehlen] = Testkarte ohne Dateien auf der Disc, nicht laden");
    out
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
