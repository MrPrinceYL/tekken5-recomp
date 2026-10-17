# Devil Within: Debug-Menüs, Testinhalte, versteckte Daten

Untersuchung des Programms `devil` (TK5DATA3-Eintrag 4, `extracted/elf/devil.elf`), Stand
17.10.2026, Branch `devil/debug`. Adressen = EE-Adressen im Programm. Quellen: `sheets/strings.csv`,
`src/raw_c/devil/*.c`, Datentabellen direkt aus der ELF gelesen.

Build-Stempel im Programm: `Feb  6 2005` `16:12:50` (0x400918 / 0x400908). Entwicklerpfad:
`sim:/home/teg/work/data/_master/` (0x400EB8), Testdaten unter `sim:romdata/...`.

## 1. Debug-Menüs (im Spiel noch vorhanden, nicht erreichbar)

### Entwickler-Modusmenü (0x103038)
Liste aus Paaren (Funktion, Name) ab 0x2AD610, Ende = 0x20CF90. Steuerung: hoch/runter, Auswahl
mit Taste 0x2000 (Kreuz).

| Funktion | Eintrag |
|---|---|
| 0x108EE8 | MAKINO MENU (Untermenü, s. u.) |
| 0x15FFF8 | SHUJI TEST |
| 0x1F7F88 | HASHI TEST |
| 0x1CBFF8 | MATTY TEST |
| 0x20A320 | KONDOH TEST |
| 0x160000 | CHARA VIEW |
| 0x108F10 | MODEL VIEW |
| 0x1344F8 | STREAM TEST |
| 0x10D5E8 | GAME LOAD START |
| 0x1038B0 | TITLE(Debug) → Debug-Titelmenü |
| 0x103888 | TITLE(Release) → normales Titelmenü |
| 0x106648 | BASE_PAK RELOAD |
| 0x125D18 | MPL RELOAD |
| 0x10D4E8 | GAME MODE |

(Namen MAKINO/SHUJI/HASHI/MATTY/KONDOH = Entwickler-Kürzel.)

**Warum unerreichbar:** Der Einstieg 0x1034A0 (setzt die Task-Funktion auf 0x103038) wird von
keinem Code und keiner Datentabelle referenziert (einziger Treffer liegt in `.eh_frame`). Der
Auslöser 0x103670, den Titel- und Modusmenü bei einer Tastenkombination aufrufen
(`(pad & 8) && (pad2 & 1)` usw.), ist im Release eine **leere Funktion** (nur `jr ra`). Der
Menücode selbst ist vollständig erhalten.

### Debug-Titelmenü „TITLE MENU (DUMMY) %s,%s“ (TITLE_MENU Zustand 5)
Tabelle 0x2B8198; erreichbar nur über „TITLE(Debug)“ (0x1038B0 → 0x103818(…, 5)). Normaler Start:
Boot-Task 0x103438 → Titel 0x103288 liest die Menünummer aus Task+0x54 (= 0 → normales Titelmenü).

| Funktion | Eintrag | Wirkung (Code) |
|---|---|---|
| 0x103B70 | NEW GAME | |
| 0x103BC0 | NEW GAME(DEVIL) | Task „NEWGAME START“, Spielmodus 5 |
| 0x103CB8 | CONTINUE | |
| 0x103D58 | LOAD GAME | |
| 0x103E40 | STAGE SELECT | 0x103DC8(…, 0) → 0x103E88, Modus 1 |
| 0x103E58 | SELECT DEVIL | 0x103DC8(…, 1), Modus 5 |
| 0x103E70 | EXTRA-DEBUG | 0x103DC8(…, 2), Modus 7 |

Dazu ein kleines Menü 0x2B8170 (vom Titel-Task in Zustand 2 gezeigt, Überschrift
„DEBUG MENU :: %s %s :: 32MB“ bzw. „128MB“ je nach DAT_002acb40 = Speicherausbau Devkit):
`DEBUG OFF` (Task+0x52 = 0), `DEGUG ON` (sic, Tippfehler im Original; Task+0x52 = 1), `OPTIONS`.

### Werkzeugliste (Tabelle 0x2B8438, vermutlich EXTRA-DEBUG / MAKINO MENU)
MAP VIEWER (0x108D60), SOUND TEST (0x133BC0, liest `soundtest.txt` bzw.
`sim:romdata/soundtest.txt`), SOUND 3D TEST (0x106948), TEXTURE VIEWER (0x109A70),
OPTION TEST (0x105E30), CHAR PROFILE LIST (0x120478), PAD VIBRATION TEST (0x102CB0),
RANK SETTING (0x10EA40).

Weitere Debug-Bausteine (Strings mit Code-Referenz): LIGHTING_TEST, LIGHT_ADJUST,
DEBUG_OBJ_LINE, SHADER TEST / SkinModelTest (`test/tsroom_02.shd`), AnimTest, EVENT_DEBUG,
PAUSE_TEST, EVENT_SAMPLE_MODE, TEST_HERO, EVENT_STREAM_TEST, TEG_SAVE_TEST, map_test_table,
Debug-Anzeigen „EffectObj Debug Disp“, „Test Object Disp“, „MapTable Debug“, „SHUJI TEST“,
„MAKINO TEST“, Ereignistyp EV_TYPE_DEBUGWORK, MT_EVENT_IMPACT_DEBUG.

## 2. Einschalten: `T5_DW_DEBUG` (abschaltbar, Standard aus)

`rust/crates/t5-recomp/src/devil_debug.rs`, über `ps2_runtime::VBLANK_HOOK`. Ohne Variable ändert
sich nichts (1:1). Mit Variable wird nur ein Datenwert gesetzt, so wie es die Debug-Pfade des
Spiels selbst tun; der übersetzte Code bleibt der des Spiels. Einmal pro Start von Devil Within,
in dem Moment, in dem der Boot-Task (Zeiger 0x2AAAF4) auf den Titel (0x103288, Zustand 0)
wechselt:

- `T5_DW_DEBUG=title`: Task+0x54 = 5 → statt des normalen Titels das Debug-Titelmenü
  (= Menüpunkt „TITLE(Debug)“).
- `T5_DW_DEBUG=dev`: Task+0x20 = 0x1034A0 → Entwickler-Modusmenü.

PowerShell:
```powershell
$env:T5_DW_DEBUG = "title"   # oder "dev"
.\play.bat
```
Erkennung: Devil Within startet per Sprung des Programmladers (kein ExecPS2), das aktuelle
Programm bleibt „loader“ → der Schalter sucht das Devil-Bild im RAM (`Machine::in_ram`). Erster
Nutzertest scheiterte genau daran (normales Menü).
Status: gebaut (`cargo check`), **im Spiel noch ungetestet**. Offen: ob die Menütexte sichtbar
sind (Debug-Schrift) und welche Testmodi ohne Entwicklerdateien (`sim:`/`host`) laufen.

## 3. Dateiverzeichnis (Testinhalte, ungenutzte Dateien)

Devil Within liest seine Daten aus `cd:tk5data2.bin` über ein eingebautes Verzeichnis:
0x146 = 326 Einträge à 16 Byte ab 0x362900 {Namens-Prüfsumme, Namensoffset in 0x363D60,
Offset, Größe} (Suche 0x20EEB0, Anzahl 0x20EEA8). Ordner: area 196, list 62, common 40,
stream 13, test 11, script 3.

Auffällig:
- **Testraum `area/makino/`** (8 Dateien: makino_000/001 .pak/.txt/_tex.pak/.evd/.evb) –
  Entwickler-Testraum (Makino), in keiner normalen Gebietsliste (Gebiete: blu_A/B, egy_A,
  lab_A/B).
- **Testfiguren** `list/chr_testchr1..3.txt`, Objekte `obj_testchr1..3`, `chr_testchr1..3`.
- **`test/`**: bgsnd*.txt, gimmick*.txt (blu/egy/lab/item/others), gim_all.bin – Testlisten für
  Hintergrundsounds und Gimmicks (Fallen/Objekte); gim_all.bin wird aus `sim:romdata/test/`
  geladen (0x1E09F0) → Entwicklerpfad.
- **`script/DEBUG/{ai,gm,mot}/readme.txt`** – Reste eines Debug-Skriptordners (18–22 Byte).
- **Bosse**: chr_boss1, chr_boss2 (+ Varianten A–D), chr_boss3, chr_boss5 – **kein boss4**
  (Lücke in der Nummerierung: gestrichener Boss möglich, nicht belegt). Modelle ogre2/3/5
  (`common/ogreN.nud/.shd`), auch hier **kein ogre4**, plus `ogre_shd.shd`.
- `area/chr/chr_000.txt~` – Editor-Sicherungsdatei, versehentlich mitgepackt.
- `common/staffroll2.txt` neben `staffroll.txt` (zweite Abspannliste?).
- Musik `stream/`: BGM_etc01/02, gameover, interlude, labo01, menu, monu, ogre02, rate, robo,
  site01, staffroll (+ stream.list) – noch nicht geprüft, ob alle im Spiel laufen.

**Offen:** Wo `tk5data2.bin` auf der Disc liegt. Auf der Disc gibt es keine TK5DATA2.BIN; an den
Verzeichnis-Offsets liegen in TK5DATA0/1/5 keine Klartextdaten (komprimiert/verschlüsselt oder
andere Basis). Die Laufzeit meldete im Spiel nur `cdvd: search \TK5DATA3.BIN`. Erst danach
lassen sich Inhalte (Texte, Testraum, Testfiguren) lesen bzw. im Spiel laden.

## 4. Noch nicht untersucht
- Inhalte der Textdateien (Dialoge, ungenutzte Texte) – hängt am Archiv (Abschnitt 3).
- Was STAGE SELECT / SELECT DEVIL / EXTRA-DEBUG (0x103E88 mit Modus 1/5/7) genau anbieten.
- Ob `area/makino` über STAGE SELECT oder MAP VIEWER ladbar ist.
- Easter Eggs (Tastenkombinationen) im normalen Spiel: noch keine Suche nach Pad-Abfragen.
