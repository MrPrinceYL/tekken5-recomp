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

### Nutzertest 17.10.: Schalter greift, aber schwarzer Bildschirm
Log: `devil: T5_DW_DEBUG=dev -> developer mode menu`, Spiel läuft weiter (Vsyncs, keine
unbekannten Sprünge). **Ursache: die Textausgabe der Debug-Menüs wurde im Release entfernt**,
die Menülogik nicht:
- Entwicklermenü 0x103038: die Schleife über die Einträge am Ende (`while (*p != 0x20CF90)`)
  hat keinen Körper mehr – dort stand die Ausgabe je Eintrag. Die Task hat keine Zeichenfunktion
  (+0x24).
- Untermenü-Widget „SUBMENU“ 0x132430 (Debug-Titelmenü, Menü 0x2B8170, MAKINO MENU): Zeichen-
  Callback +0x2C = 0x132618 ist `jr ra; nop`, in der Eintragsschleife 0x1325EC stehen 4 `nop`
  an Stelle des Ausgabeaufrufs.
→ Die Menüs funktionieren blind (Steuerkreuz hoch = Bit 0x10, runter = 0x40, Auswahl Kreuz
0x2000), zeigen aber nichts. Blind im Entwicklermenü (Nulleinträge werden übersprungen):
0 MAKINO MENU, 1 SHUJI, 2 HASHI, 3 MATTY, 4 KONDOH, 5 CHARA VIEW, 6 MODEL VIEW, 7 STREAM TEST,
8 GAME LOAD START, 9 TITLE(Debug), 10 TITLE(Release), 11 BASE_PAK RELOAD, 12 MPL RELOAD,
13 GAME MODE.
**Overlay (Nutzerwunsch 17.10.):** `devil_debug::menu_text` liest bei jedem Vsync die offenen
Menüs aus dem Gast-RAM (Task-Pool 0x2A9A30, Kette über +0x18): Entwicklermenü (Funktion
0x103038, Zustand 1, Tabelle +0x38 bis 0x20CF90, Cursor +0x40) und jedes SUBMENU (Funktion
0x1324B8 = Update aus dem Init-Callback 0x1325D8, Zustand 0, Tabelle +0x38 bis Funktion 0,
Titel +0x3C, Cursor +0x40). t5-bevy zeigt den Text oben links (`DebugMenuText`), nur mit
`T5_DW_DEBUG`. Ersetzt die entfernte Ausgabe, ändert keinen Spielzustand. Nutzertest offen.

**Steuerung (aus dem Eingabecode, Pad-Wörter ab 0x2A98E0, libpad-Bits):** hoch 0x10 / runter
0x40 (Steuerkreuz), **Entwicklermenü wählt nur mit KREIS (0x2000)**, SUBMENU mit KREIS oder
KREUZ (0x4000). Kein „Zurück“: das Entwicklermenü wartet in Zustand 2, bis die Task des
gestarteten Modus endet, und erscheint dann wieder. Die ursprüngliche Tastenkombination fürs
Entwicklermenü war **SELECT halten + START** (0x103038 am Ende: Pad 0x2A98E0 & 1 und 0x2A98E4 & 8,
dann der geleerte Aufruf 0x103670). Auf dem Titel (0x103288) setzt START+SELECT halten auf den
Titel zurück. Das Overlay zeigt die Tasten unter jedem Menü (Tastatur: C = Kreis, X = Kreuz).

## 2a. Alle Einträge geprüft (17.10.)

Jede Menüfunktion aus den Tabellen 0x2AD610, 0x2B8170, 0x2B8198, 0x2B8438 gelesen. „Entfernt“ =
Funktion besteht nur aus `jr ra; nop` (Code im Release gelöscht, nicht rekonstruierbar ohne zu
erfinden). Tasten aus dem Eingabecode des jeweiligen Modus (Pad-Wörter 0x2A98E0 gehalten,
+4 gedrückt, +8 Wiederholung; Sticks 0x2A98A0).

| Eintrag | Funktion | Ergebnis |
|---|---|---|
| MAKINO MENU | 0x108EE8 | Untermenü (Tabelle 0x2B8438, = Werkzeugliste) |
| SHUJI TEST | 0x15FFF8 | **entfernt** |
| HASHI TEST | 0x1F7F88 | Task „HASHI_TEST_MENU“, alle Callbacks `jr ra` → endet nie, Menü wartet ewig (**hängt**) |
| MATTY TEST | 0x1CBFF8 | Task „MATTY_MENU“ nur mit Zeichenfunktion 0x1CBF70, L1 beim Start = Variante; ungeprüft, ob nutzbar |
| KONDOH TEST | 0x20A320 | **entfernt** |
| CHARA VIEW | 0x160000 | **entfernt** |
| MODEL VIEW | 0x108F10 | funktioniert: Liste (hoch/runter, Kreis/START) → Ansicht (Sticks, L1/R1 Zoom, L2/R2 Wert 0x2B849C, Dreieck/Viereck Umschalter, START Kamera, Kreuz/SELECT zurück) |
| STREAM TEST | 0x1344F8 | funktioniert, braucht Liste 0x405D60: hoch/runter Stream (Kreis = 10er), links/rechts Lautstärke, Kreis/Dreieck/Viereck abspielen, Kreuz/SELECT stoppen, START Pause |
| GAME LOAD START | 0x10D5E8 | GAME MODE + Startzustand 8 |
| TITLE(Debug) / TITLE(Release) | 0x1038B0 / 0x103888 | Debug- / normales Titelmenü |
| BASE_PAK RELOAD | 0x106648 | lädt base.pak neu (Task 0x106690), keine Eingabe |
| MPL RELOAD | 0x125D18 | lädt Gebietsliste neu, sofort |
| GAME MODE | 0x10D4E8 | normales Spiel („GAME MAIN“), übernimmt DEBUG ON (Task+0x52) als +0x5C |
| DEBUG OFF / DEGUG ON | 0x103720 / 0x103740 | Task+0x52 = 0 / 1 |
| OPTIONS / OPTION TEST | 0x105E30 | Eingabetest: registriert Steuerkreuz + Kreuz (Anzeige entfernt) |
| 開発モード (Kaihatsu-Modus) | 0x103768 | ruft den geleerten Auslöser 0x103670 → tut nichts |
| NEW GAME / NEW GAME(DEVIL) | 0x103B70 / 0x103BC0 | Stufe 0–3 (hoch/runter), Kreis startet (Controller 1 oder 2) |
| CONTINUE / LOAD GAME | 0x103CB8 / 0x103D58 | Spielstand fortsetzen / laden |
| STAGE SELECT / SELECT DEVIL / EXTRA-DEBUG | 0x103E40/58/70 → 0x103E88 | Stage 0–4 (5 = zurück), Kreis → Stufe 0–3, Kreis startet (Modus 1 / 5 / 7), Kreuz zurück |
| MAP VIEWER | 0x108D60 | Karte hoch/runter, Kreis lädt (nur vorhandene) → EVENT_SAMPLE_MODE: freie Kamera (Sticks, R2/L2 hoch/runter, Kreis schneller, R3 Reset) |
| SOUND TEST | 0x133BC0 | Bank links/rechts (0–20), Ton hoch/runter, Kreis halten spielen, Kreuz stopp, Viereck Bank laden, Dreieck alle aus |
| SOUND 3D TEST | 0x106948 | lädt EVENT_SAMPLE_MODE (wie MAP VIEWER) |
| TEXTURE VIEWER | 0x109A70 | links/rechts blättern (Dreieck x256, Kreis x16), Kreuz Ansicht 0–3, R1 Speicher 0–7 |
| CHAR PROFILE LIST | 0x120478 | hoch/runter blättern |
| PAD VIBRATION TEST | 0x102CB0 | **entfernt** |
| RANK SETTING | 0x10EA40 | **entfernt** |

Werkzeuge haben keinen Zurück-Weg im Code; das Entwicklermenü wartet, bis die Task des Modus endet.

**Overlay** (`devil_debug::overlay`): zeigt jedes offene Menü (Einträge, Cursor, `[entfernt]`,
`[haengt]`) und jeden laufenden Modus aus der Tabelle mit aktuellen Werten und Tasten.

**Reparatur (nur mit `T5_DW_DEBUG`):** Nach dem Ende eines Modus zeigt sich das Entwicklermenü
mit Eintragsanzahl 0 (0x102FD8 löscht +0x3C, gezählt wird nur in Zustand 0, den der ungenutzte
Wiedereinstieg 0x1034A0 auslösen würde) → nächstes hoch/runter = Endlosschleife 0x1031E8
(Nutzerbericht: Hänger nach CHARA VIEW). Der Hook setzt dann Zustand 0 (neu zählen), wie 0x1034A0.

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

**Archiv gefunden (18.10.):** `cd:tk5data2.bin` = **TK5DATA2.BIN** (LBA 2059640, 244 426 640 Byte,
letzte Datei der Disc; Pfad: EE 0x20E4C0 → IOP NU_File_Manager 0x0B2A68 macht daraus
`\TK5DATA2.BIN;1` → sceCdSearchFile). Inhalt **unverschlüsselt und unkomprimiert**, Offsets aus
dem Verzeichnis gelten ab Dateianfang. (Meine frühere Aussage „verschlüsselt“ war falsch: ich
hatte in TK5DATA0/1/5 gesucht.) Entpacken: `tools/dw_extract.py <iso> <extracted>` →
`extracted/tk5data2/` (nicht in Git) + `sheets/dw_files.csv`.

## 3a. Bosse, Ogres, gestrichene Inhalte (aus TK5DATA2)

**Boss-Zuordnung** (Bossräume `area/<gebiet>/<gebiet>_005.txt`, Entwickler-Warpliste
`area/mpl.txt`, Shift-JIS):

| Gebiet | mpl-Name | Bossraum | Boss |
|---|---|---|---|
| lab_A | 1 研究室A (Labor A) | 1-5 ボス | boss1 (`kmb1`: Maschinengewehr, Schrotflinte, Sprungschuss) |
| lab_B | 2 研究室B (Labor B) | 2-5 ボス | boss2 (Bewegungen aus Heihachi/Kazuya-Sätzen: `hei`, `he`, `kz`) |
| egy_A | 3 エジプトA (Ägypten A) | 3-5 ボス | boss3 = **Ogre** (Soundset `c_03_ogre`) |
| blu_A | 4 蒼い遺跡A (Blaue Ruinen A) | 4-5 ボス | **kein Boss**: nur sechs normale Monstertypen |
| blu_B | 5 蒼い遺跡B (Blaue Ruinen B) | 5-5 ボス | boss3 (Ogre) und danach boss5 = **True Ogre** (Soundset `c_05_ogre` mit `METAMO` = Verwandlung, `bhb_ai_boss5_lv1/lv2`, `bhb_mo_boss5_start2`) |

- **Fehlender Boss 4:** Die Entwickler-Warpliste nennt für jedes der fünf Gebiete einen
  Bossraum („x-5 ボス“). In Gebiet 4 (blu_A) steht dort keine Boss-KI, kein `init_obj_boss4`, keine
  Liste `chr_boss4`, kein Modellplatz in `common/char_model_all.txt`. Die Nummerierung 1, 2, 3, 5
  und der leere Bossraum in Gebiet 4 sprechen für einen **gestrichenen vierten Boss**. Daten davon
  sind auf der Disc nicht vorhanden, er lässt sich also nicht wiederherstellen.
- **Ogres:** Es gibt nur **zwei** Ogre-Kämpfe (Ogre = boss3, True Ogre = boss5). Der Code lädt nur
  `common/ogre3.shd` und `ogre5.shd` (0x113A18, mit `simple.shd`, `tkchr.shd`). `ogre2.nud/.shd`,
  `ogre3.nud`, `ogre5.nud` und `ogre_shd.shd` lädt weder Code noch eine Liste → **Überbleibsel**.
  `ogre2` passt zur Bossnummer 2, liegt aber neben einem boss2 mit Heihachi/Kazuya-Bewegungen;
  dass boss2 früher ein Ogre war, ist möglich, aber nicht belegt. Die Musik `BGM_ogre02`
  läuft in blu_B.
- **boss2 A–D:** vier Listen, die sich nur im Modell unterscheiden (`nut_chr_boss2_A..D`), belegen
  die Modellplätze 1–4 in `char_model_all.txt`; kein Gebiet lädt sie direkt.
- **Testfiguren testchr1–3:** Bewegungslisten vorhanden (testchr1 = boss2 + `shoryu01/24`,
  `ozhe_power`, `nahe_*`; testchr2 = boss2 + Devil-Jin-Flügel `djin_newhane`), Modellplätze 5, 6, 15.
  Ihre Modelle (`obj_obj_testchr*`, `nut_chr_testchr*`) stehen in **keiner** Datei → nicht ladbar.
- **Test-Items:** `common/item.txt` (`itm_testA`), `test/gimmick_item.txt` (Gold-/Lebens-Items
  „luminous“ S/M/L).
- **Abspann:** `staffroll2.txt` lädt `asb/ast_staffroll_test.dat` (Test-Abspann).
- `script/DEBUG/*/readme.txt`: „script data for ai“ usw.

**Entwickler-Warpliste `area/mpl.txt` (81 Einträge, Index = MAP VIEWER):** 0–48 die fünf Gebiete
(je Raum, Bossraum, Speicherraum), dazu Testkarten: `bgtest` 1–4 Musterkarten von Yamauchi
(Labor, Ägypten, Blaue Ruinen und **„林サンプル“ = Wald**, ein Gebiet, das es im Spiel nicht gibt),
`chr` 1–5 (Figuren-Prüfraum, Figurentest, Figurentest Tekken / Monster / Desert), `effect`
(Effekt-Prüfkarte), `kondoh` 1–2, `makino` 1, `matty` 1–6, `mine` 1–2, `szk` 1–11 (test_egy,
test_blu, test_gim, 100x100, test_unit, test_1m, Lastprüfung, Wendeltreppen-Tests …).
Dateien gibt es nur für `chr` und `makino` (plus die fünf Gebiete); die übrigen Testkarten
(darunter der Wald) fehlen auf der Disc.

**Im Spiel testen:** `T5_DW_DEBUG=dev` → MAKINO MENU → MAP VIEWER. Das Overlay zeigt die Warpliste
mit deutschen Namen und markiert Karten ohne Daten mit `[Daten fehlen]`. Ladbar sind die Gebiete,
die `chr`-Testräume (Index 53–57) und der Makino-Testraum (61).

## 4. Noch nicht untersucht
- Inhalte der Textdateien (Dialoge, ungenutzte Texte) – hängt am Archiv (Abschnitt 3).
- Was STAGE SELECT / SELECT DEVIL / EXTRA-DEBUG (0x103E88 mit Modus 1/5/7) genau anbieten.
- Ob `area/makino` über STAGE SELECT oder MAP VIEWER ladbar ist.
- Easter Eggs (Tastenkombinationen) im normalen Spiel: noch keine Suche nach Pad-Abfragen.
