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

**Overlay** (`devil_debug::overlay`, Texte auf Englisch, Nutzerwunsch 18.10.): zeigt jedes offene Menü (Einträge, Cursor, `[removed]`,
`[hangs]`) und jeden laufenden Modus aus der Tabelle mit aktuellen Werten und Tasten.

**Reparatur (nur mit `T5_DW_DEBUG`):** Nach dem Ende eines Modus zeigt sich das Entwicklermenü
mit Eintragsanzahl 0 (0x102FD8 löscht +0x3C, gezählt wird nur in Zustand 0, den der ungenutzte
Wiedereinstieg 0x1034A0 auslösen würde) → nächstes hoch/runter = Endlosschleife 0x1031E8
(Nutzerbericht: Hänger nach CHARA VIEW). Der Hook setzt dann Zustand 0 (neu zählen), wie 0x1034A0.

## 2b. Mod-Menü (nur Branch `devil/debug`, nur mit `T5_DW_DEBUG`, nicht 1:1)

`t5-recomp/src/devil_mod.rs`. Öffnen/Schließen: SELECT halten + L1; mit SELECT gehalten:
hoch/runter Eintrag, links/rechts Auswahl, KREIS ausführen.

**Belegte Strukturen** (Funktionsnamen aus den Debug-Strings der Bibliothek `tsEHpMan*`):
- Figuren („man“, erzeugt von 0x148400 → 0x148070): Tasks Typ 1000 mit Update 0x1493F8 im
  Task-Pool. +0x1C Name, +0x140 Index Eingabe-Datensatz, +0x144 Index Figurdaten, +0x148 KI,
  +0x154 Bit 2 = KI-gesteuert (nur bei Erzeugung und Tod ausgewertet: ohne Bit → Game Over
  0x10E0C8, mit Bit → besiegter Gegner 0x11F9B0), +0x160 Modell, +0x162 Pad-Port.
- Eingabe-Datensatz (0x15C6A0, Pool *0x42B6FC, Schritt [0x42B6F0], Anzahl [0x42B6EA]):
  +0 Controller: 0 = Pad 1 (0x15C4E0), < 0 = KI-Puffer [0x41BED0] (die KI jeder Figur schreibt
  ihn vor deren Update, 0x15E5F8), sonst keine Eingabe. Gegner nutzen also denselben
  Eingabeweg wie der Spieler.
- Spieler („ManMgr“ 0x146FB0, [0x41BB10]+0x140 → Eintrag *0x42B66C): Eintrag +0 = Spielerfigur
  (0x147A98); Kamera, HUD und Gegner-Ziel folgen ihr.
- Figurdaten (0x16EF88): +0x3004 Lebensverwalter: Leben +0x301C / Max +0x3020, Team +0x300A,
  Devil-Leiste +0x3024 / Max +0x3028; Positionsmatrix +0x70, Yaw +0x110.

**Einträge:**
- **Switch character** (links/rechts Ziel = jede lebende Figur der Szene): Ziel-Controller = 0,
  bisheriger Spieler = keine Eingabe, ManMgr → Ziel, optional Team des Spielers fürs Ziel
  („Team swap when switching“, Standard an). Damit sind auch Gegner und Bosse steuerbar: sie
  bekommen die Pad-Eingabe statt der KI-Eingabe über denselben Weg. Rückweg per Eintrag oder
  automatisch, wenn das Ziel verschwindet.
- **Unlimited health / devil gauge** (gesteuerte Figur, je Vsync Wert = Max).
- matty-Cheats: Restore 30% health, Set/Clear COLLECT flags.
- **Position probe:** zeigt Kandidaten der Position (Matrix +0x30, Yaw, obj+0x16C) – Grundlage für
  Teleport/No-Clip.

**Noch nicht gebaut (Begründung):**
- Teleport / No-Clip: maßgebliche Position nicht belegt (Matrix +0x70 vs. Bewegungs-Engine
  0x165F80/0x16D728); erst Messung mit „Position probe“ im Spiel.
- Spawn Gegner/Bosse/Objekte/Items: Erzeugung 0x148400 braucht einen Spawn-Datensatz aus den
  Ereignisdaten und im Raum geladene Figurdaten; dafür Ereignisweg `EV_TYPE_OBJ`/`init_obj`
  analysieren und Gast-Aufrufe mit Argumenten.
- Stage select: vorhanden im Debug-Titelmenü (`T5_DW_DEBUG=title` → STAGE SELECT).
- Figuren aus dem normalen Tekken 5 (z. B. Kazuya): andere Engine und Formate (TK5DATA0/1) –
  Portierung nicht machbar ohne eigenes Großprojekt; nächstliegend: boss2 (Kazuya-/Heihachi-
  Bewegungen) per Switch character.

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
| blu_A | 4 蒼い遺跡A (Blaue Ruinen A) | 4-5 ボス | Arena-Boss `BossStage04` (Kern, Kristalle, Satelliten, Barriere) + Monsterwellen |
| blu_B | 5 蒼い遺跡B (Blaue Ruinen B) | 5-5 ボス | boss3 (Ogre) und danach boss5 = **True Ogre** (Soundset `c_05_ogre` mit `METAMO` = Verwandlung, `bhb_ai_boss5_lv1/lv2`, `bhb_mo_boss5_start2`) |

- **Boss von Gebiet 4 (korrigiert 18.10.):** Der Bossraum 4-5 hat keinen Figuren-Boss, aber eine
  Boss-Arena: `blu_A_005.pak` enthält `obj_bg_blu_BossStage04_C_00`, Kern (`Core`), Kristalle
  (`Cry`), Satelliten (`Sat`), Barriere und Trefferzone `hitBossStage04_A`, und `blu_A_005.evb`
  setzt sie ein (dazu Monsterwellen). Die Nummerierung boss1/2/3/5 ohne boss4 passt dazu: Boss 4
  ist das Arena-Objekt. Die frühere Vermutung „gestrichener Boss 4“ ist damit **widerlegt**.
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

**Paketformat BDPK** (Prüfung 0x109F80, Einträge 0x10A5A8): Kopf 0x80 Byte (+4 Anzahl), dann je
0x80 Byte {`BD`, Typ u16, u32, Größe, ?, Offset, Name[0x6C]} – Namen ohne `.dat`. Inventar aller
Pakete: 1022 Einträge (`extracted/dw_pak_inventory.txt`, nicht in Git).

**Verlangt, aber auf der Disc nicht vorhanden** (Listen `.txt` gegen Paketinventar; die `nut_`-
Dateien ausgenommen, die nicht einzeln verpackt sind):
- **Testfiguren testchr1–3:** Modelle und Startobjekte (`obj_obj_testchr*`, `gym_init_obj_testchr*`).
- **Ogre (boss3):** die Bewegungen `gym_Jb2_horn00`, `Jb2_tail01`, `Mb2_kamae`, `Pb2_snep2`,
  `Pb2_wpat`, `Yb2_FIRE2` (Horn, Schwanz, Feuer) stehen in `egy_A/boss.txt`, fehlen aber →
  gestrichene Ogre-Angriffe. **True Ogre:** KI `bhb_ai_boss5_lv2` fehlt (nur lv1 vorhanden).
  boss2: Bewegung `gym_ozhe_1ckj66a` fehlt.
- **Skripte:** `bhb_ai_enemy00`, `bhb_mo_hero`, **`bhb_mo_multi_start`** (Mehrspieler-Start?),
  alle Testkarten-Skripte (bgtest, kondoh, matty, szk).
- **Testkarten:** keine Dateien für bgtest (inkl. Wald), effect, kondoh, matty, mine, szk; die
  `chr`-Testräume haben zwar Listen, aber ihre Raumgeometrie (`obj_bg_lab_roomC…`,
  `obj_bg_*_roomA/B…`) fehlt → nicht sinnvoll ladbar. Der Makino-Testraum (`makino_001.pak`) ist
  vollständig und enthält das Objekt `obj_obj_default_luminous_test`.
- **Gimmicks:** Schlüssel/Siegel-Türen, Zäune, Pfähle, Pendel, `lab_pathAnimJack`, Testobjekte
  `itm_testA`, `default_testA`, `matty_moving_floor…` (nur in den Testlisten).
- **Test-Abspann** `asb/ast_staffroll_test`.
Eine Suche über die gesamte Disc (Klartext) findet diese Namen nur in den Listen selbst.

**Vorhanden und im normalen Spiel genutzt:** StarBlade-Item `obj_obj_item_starblade_p00_d00`
(`blu_A_002.pak`, Ereignis `gim_item_starblade_p00_000001` in `blu_A_002.evb`), leuchtende
Gold-/Lebens-Items S/M/L.

**Vorhandene, aber ungenutzte Figuren? (18.10., Nutzerfrage)** Alle 60 Figurenplätze aus
`common/char_model_all.txt` gegen das Paketinventar und die Gebietsdateien geprüft:
- Jin (0) und Devil Jin (7): Spielfigur, `base.pak`.
- boss2 (1–4, Varianten), boss1 (56), boss3 (58), boss5 (59): je in ihrem Bossraum geladen.
- Alle Gegnertypen `tk_*` (standard1/2, speed1, tricky1, power1/2) und `mon_*` (Monster:
  standard1/2, speed1, tricky1, power1/2) haben Modelle **und** werden in Gebieten geladen
  (tk_*: Labor A/B, Ägypten; mon_*: Ägypten, Blaue Ruinen A/B).
- testchr1–3 (5, 6, 57): **keine** Daten.
→ **Es gibt keine Figur, deren Daten vorhanden sind, die aber in keinem Level vorkommt.**
Bei Objekten (Türen, Schlüssel, Kisten, Pendel …) lässt sich das per Namenssuche nicht sicher
sagen: Platziert werden sie über Instanznamen (`gim_…_000001`) und `common/gim.db`, deren Format
noch nicht entschlüsselt ist. Auffällig: Debug-Grundkörper `obj_obj_CUBE` / `obj_obj_SPHERE` in
`base.pak` (der Code kennt `primitive_sphere` / `primitive_cylinder`).

**Namen der Gegner/Bosse (18.10.):** Es gibt keine Anzeigenamen (`common/char_prof.bin` = nur
interne Kennungen `chr_…`, Texte im Spiel sind Schrift-Grafiken `cap_teg_msg_font_*`). Zuordnung über
die Soundsets (Paket → Raum):
| Kennung | Soundset | Bedeutung |
|---|---|---|
| boss1 (`kmb1`, Labor A) | `snd_c_boss` | allgemeiner Boss-Satz |
| boss2 (Labor B, Varianten A–D) | `snd_c15hei` | **Heihachi**-Sounds, Heihachi/Kazuya-Bewegungen |
| boss3 | `snd_c_03_ogre` | **Ogre** |
| Arena Gebiet 4 | `snd_c_04_iseki` („iseki“ = Ruine; Einträge ISEKI_001…, `KOWASI` = zerstören) | Ruinen-Arena |
| boss5 | `snd_c_05_ogre` (+ `METAMO`) | **True Ogre** (Verwandlung) |
| `tk_*` (Labor A/B, Ägypten) | `snd_c_tks` + `snd_c21jac` / `c_jac_f` | Soldaten mit **Jack**-Sounds |
| `mon_*` (Ägypten, Ruinen) | `snd_c_monst` | Monster |
| Jin / Devil Jin | `snd_c10jin` / `snd_c30dvj` | Spielfigur |
Varianten heißen intern standard1/2, speed1, tricky1, power1/2, je in Stufen lv1–lv4.
Nebenbei: Der Code kennt sechs Sprachen (ENGLISH, DEUTSCH, FRANCAIS, ESPANOL, ITALIANO, BRITISH) und
Boot-Pakete boot_jp/us/du/fr/es/it/uk/ko; auf der US-Disc liegt nur `boot_us`.

**Aufnehmbare Items (Platzierungen `gim_item_*` in allen `.evb`/`.evd` gegen die Modelle):**
platziert sind `item_collect` (83×), Gold S/M/L, Leben S/M, StarBlade (Raum 4-2). **Nie platziert**,
aber als Modell vorhanden: `item_life_L` (großes Lebens-Item, `base.pak`) und alle „luminous“-
Varianten (Gold/Leben S/M/L). Ob der Code sie als Beute erzeugt (der Code rechnet Gold aus der Stufe
der Gegner, 0x148F20), ist **nicht belegt**. Test-Items `itm_testA` fehlen ganz.

**Objekte, die nie platziert werden** (Modell in einem Paket, kein `.evb`/`.evd` nennt es):
`BossStage04_B` und `BossStage04_barrier` (im Paket des Bossraums 4-5, während Kern, Kristalle und
Satelliten dort platziert sind), Varianten `p01 a00/a01` der Schlüssel-/Siegeltüren (lab, egy, blu),
`blu_boxA_mov_p00_a00`, Pfahl-Varianten `poleC … a01`, `default_effect_test`, `default_luminous_test`
(nur Makino-Testraum), Debug-Grundkörper CUBE/SPHERE. Dynamisch erzeugt und daher nicht ungenutzt:
Figuren, Boss-Effekte `BossEffNN`, Kartensymbole `map_*`, Zielmarker `lockonMarker`, Schatten.

**MATTY TEST = Cheat-/Debug-Menü „matty debug“** (Baumansicht „Treeview“, Knoten à 0x30 Byte ab
0x356130; `mt_ndebug_util_list.c`): Zweig **CHEAT** mit „GIM: Gold Passport“ (Funktion 0x1D93B8,
im Release **geleert**) und „体力30％回復“ = **30 % Leben auffüllen** (0x1D92B0, **funktioniert**: Leben
+ Max × Faktor), Zweig „DEBUG matty“ mit global flag / register / system register list,
**auto-attack** (MT_DEBUG_AUTO_ATTACK 0x1D9970), „impact STR hack“, „Set / Clear COLLECT flags“;
dazu Einträge FORCE MENU, PAUSE MENU, STAGE RESULT, TOTAL RESULT, MAIN MENU. Die Zeichenfunktion
(0x1CBF70) räumt nur auf; der eigentliche Antrieb der Baumansicht (Eingabe/Zeichnen 0x1D8260,
0x1D88B0) wird von nichts mehr aufgerufen (nur `.eh_frame`-Treffer) → MATTY TEST zeigt nichts.

**Cheat-Menü im Spiel (18.10., `T5_DW_DEBUG`):** SELECT halten + L1 öffnet/schließt, SELECT + hoch/
runter wählt, SELECT + KREIS führt aus. Einträge = die Knoten des matty-Baums (Namen aus dem
Programm). Ausführbar sind die Funktionen ohne Parameter: **Restore 30% health** (0x1D92B0),
**Set COLLECT flags** (0x1D9A78: Flags 0x200–0x227 = 1, alle Sammelobjekte als gefunden – wird mit
dem Spielstand gespeichert!) und **Clear COLLECT flags** (0x1D9AF0). Nicht verfügbar: Gold Passport
(geleert), auto-attack, impact STR hack und die Listen (brauchen den Kontext der entfernten
Baumansicht). Aufruf über `ps2_runtime::kernel::request_guest_call`: die Laufzeit ruft die
Spielfunktion am nächsten Interrupt-Punkt wie einen Interrupt-Handler des Spiels auf (eigener
Stack, gp des unterbrochenen Threads). Nutzertest offen.

**Entwickler-Warpliste `area/mpl.txt` (81 Einträge, Index = MAP VIEWER):** 0–48 die fünf Gebiete
(je Raum, Bossraum, Speicherraum), dazu Testkarten: `bgtest` 1–4 Musterkarten von Yamauchi
(Labor, Ägypten, Blaue Ruinen und **„林サンプル“ = Wald**, ein Gebiet, das es im Spiel nicht gibt),
`chr` 1–5 (Figuren-Prüfraum, Figurentest, Figurentest Tekken / Monster / Desert), `effect`
(Effekt-Prüfkarte), `kondoh` 1–2, `makino` 1, `matty` 1–6, `mine` 1–2, `szk` 1–11 (test_egy,
test_blu, test_gim, 100x100, test_unit, test_1m, Lastprüfung, Wendeltreppen-Tests …).
Dateien gibt es nur für `chr` und `makino` (plus die fünf Gebiete); die übrigen Testkarten
(darunter der Wald) fehlen auf der Disc.

**Im Spiel testen:** `T5_DW_DEBUG=dev` → MAKINO MENU → MAP VIEWER. Das Overlay zeigt die Warpliste
mit englischen Namen und markiert Karten ohne Daten mit `[no data]` (bzw. `[no room geometry]` für die chr-Räume). Vollständig ladbar sind die fünf Gebiete und der Makino-Testraum (61); den `chr`-Testräumen
(53–57) fehlt die Raumgeometrie.

## 4. Noch nicht untersucht
- Inhalte der Textdateien (Dialoge, ungenutzte Texte) – hängt am Archiv (Abschnitt 3).
- Was STAGE SELECT / SELECT DEVIL / EXTRA-DEBUG (0x103E88 mit Modus 1/5/7) genau anbieten.
- Ob `area/makino` über STAGE SELECT oder MAP VIEWER ladbar ist.
- Easter Eggs (Tastenkombinationen) im normalen Spiel: noch keine Suche nach Pad-Abfragen.
