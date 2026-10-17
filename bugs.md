# Bugliste (Nutzerbericht 14.10.2026) + Prioritäten

Status: [ ] offen, [~] in Arbeit, [x] behoben (mit Ursache).

## P1 Eingabe (schnell, kann Folgebugs erklären)
- [x] B1 Linker/rechter Stick falsch verknüpft: Der linke Stick wird als Steuerkreuz gewertet.
      - Ursache: Das Frontend schrieb nie Analogwerte (`PadShared::sticks` blieb 0 = Mitte), und
        der linke Stick war per Standardbelegung auf die D-Pad-Tasten gelegt.
      - Fix: neue Steuerelemente „Left/Right Stick Up/Down/Left/Right“ → Bytes RX RY LX LY
        (`Input::sticks`, Totzone 0,12).
      - controls.json v2: Die Migration entfernt `Axis:LeftStick*` vom D-Pad.
- [ ] B2 StarBlade: Das Zielvisier kehrt nach Loslassen des Sticks nicht in die Mitte zurück
      (vermutlich Analogwerte/Neutralstellung, siehe B1).
- [ ] B3 Options → Game Options: Auf „Rounds“ ändert Links/Rechts die Difficulty
      (Eingabe oder Menülogik prüfen, evtl. B1).
- [x] B15 Stick-Bewegungen werden vom Spiel nicht erkannt (Nutzer 17.10., unter Linux
      getestet). **Ursache (17.10.):** padman-HLE schrieb pad_data +0x64 modeConfig = 1. Das
      echte padman (in main `.dmh` eingebettet, Code +0x2840) setzt 2, wenn der Controller die
      Konfig-Abfrage beantwortet (DualShock 2), 1 erst nach 10 Fehlversuchen. libpad
      `scePadInfoMode(CUREXID)` (0x10C648) liefert bei 1 → 0, die Pad-Statusmaschine des Spiels
      (0x239658) ruft dann nie `scePadSetMainMode(analog)` auf, und 0x239500 setzt die Sticks
      im Digitalmodus fest auf 0x80. Fix: modeConfig = 2 (`iop/pad.rs`). Nutzer: T5 + Sticks ok.
      - Folgefehler: StarBlade / Devil Within danach ganz ohne Tasten. Sie gehen jetzt den
        Analog-Weg (SetMainMode, EnterPressMode = SetButtonInfo, SetActAlign); libpad liest das
        Ergebnis je Kommando woanders (0x06/0x08 → +0x14, 0x0A → +0x10, 0x0B → +0x1C,
        0x14 → +0x08, sonst +0x0C), die HLE schrieb immer +0x0C → Devil Within (0x22A910)
        blieb vor scePadRead stehen. Fix: Ergebnis an die gelesene Stelle. Nutzertest 17.10.:
        Tasten in T5, StarBlade, Devil Within ok (Arcade History ungetestet, hängt an B12).

## P2 Ton
- [x] B4 Kampf: keine Musik. (Nutzer 17.10.: Musik da)
- [x] B5 Story-Prolog: keine Musik, keine Stimmen. (Nutzer 17.10.: geht)
- [x] B6 Story-Zwischensequenzen (z. B. Kazuya Stage 4): keine Stimmen, keine Musik.
      (Nutzer 17.10.: geht)
- [x] B7 Manche Intro-Posen ohne Stimme. (Nutzer 17.10.: geht)
- [x] B8 Intro-Video: stumm, außerdem spielt ein falscher Sound. (Nutzer 17.10.: Ton da)
- [ ] B13 FMVs (Intro-Video, Zwischensequenzen): Ton nicht immer synchron zum Bild
      (Nutzer 17.10.).

## P3 Grafik
- [ ] B9 MODE SELECT: Alle Einträge gelb statt weiß/grau. Beschreibungstexte aller Einträge
      liegen übereinander (Vergleichsbild PCSX2).
- [ ] B10 FMVs: Bildartefakte (Nutzer 17.10.: weiterhin, alle FMVs).
- [x] B11 Polar Paradise (Pinguin-Stage): Logik 60, aber nur 30 gerenderte Bilder/s.
      - Ursache (17.10.): MTVU-Worker > 16,7 ms pro Bild, weil Polar-VU-Mikrocode nie
        gesammelt war und Paar für Paar lief. Details + Messwerte: CLAUDE.md Fortschritt.
      - Fix 17.10.: Mikrocode gesammelt (gen_vu.py --collect / --collect-shapes), Worker
        max. 14,0 -> 10,2 ms / Bild. Nutzertest im Fenster: läuft flüssig.
- [ ] B14 Effekte flackern (Nutzer 17.10.): Hit Sparks, Jinpachis Effekte, Sun Flare allgemein
      (Pool-Stage, Polar Paradise, ...). **Ursache Timer-1-Fehler in `timers.rs`, behoben 17.10.
      (siehe unten) – Nutzertest offen** (Flare, Hit Sparks, Jinpachi: alle laufen über dasselbe
      Effektbudget).
      Analyse Sun Flare 17.10. (Linux, Polar = `stage.sh 10` mit dem ALTEN Spielstand: seit dem
      freigeschalteten Spielstand (32bf0cbe) ist 10 = Acid Rain → `T5_MEMSRC=<memcard-Kopie mit
      port0/BASLUS-21059/old/>`; `T5_BURST=75` = jedes Bild 1 s lang, Ergebnisse in `run/flare/`):
      - Reproduziert headless: in einzelnen Bildern fehlt der GANZE Flare (Strahlen + Kreise),
        nicht nur blasser. Messung: Helligkeit oberes linkes Bilddrittel 150,4/150,8 normal,
        145,6/145,9 ohne Flare.
      - GPU-Raster: 5 von 61 Bildern ohne Flare, in zwei Läufen EXAKT dieselben Bilder (4488,
        4508, 4515, 4521, 4522) → deterministisch, kein Timing. CPU-Raster (`T5_GPU=0`): anderes
        Muster (~60 % ohne) - vermutlich nur, weil der langsamere Lauf die Kämpfer anders stehen
        lässt (`--press` in Host-Sekunden), nicht widerlegt.
      - Kein Verdeckungstest per Rücklesen: `T5_DLLOG=1` (protokolliert jede GS→EE-Übertragung,
        `gs.rs local_to_host`) zeigt im ganzen Lauf 0 Rücklesungen.
      - GS-Mitschnitt (`T5_GSCAPTURE=75`, ausgewertet mit scratch-Skript gsprims2.py): die
        Flare-Elemente sind Quads (je 2 Dreiecke), ALPHA 0x48 (Cs*As+Cd, additiv), TEST 0x5000f
        (Alpha != 0, Z >=), Textur tbp 11200/11232/11264, Helligkeit im Vertex-Alpha. In den
        Halbbildern 18, 25, 31, 32 (Abstände = die dunklen Bilder) FEHLEN diese Quads komplett
        im GS-Strom; Muster wiederholt sich ~alle 41 Bilder (59, 65/66, 72/73; 93, 100, 106/107).
        → Nicht der Raster: der Spielcode (EE/VU1) schickt den Flare in diesen Bildern nicht.
      - EE-FPU / VU-Semantik (`ee-core/fpu.rs`, `vu0.rs`) geprüft: kein IEEE, Überlauf geklemmt,
        Denormale = 0 - keine offensichtliche Ursache. (PCSX2 GameIndex SLUS-21059: eeClampMode 2
        „Fixes camera …“.)
      - **Ursache gefunden (17.10.): das Effekt-Lastbudget des Spiels.** Weg: Flare = PATH1 (VU1-
        XGKICK, `T5_TBPWATCH=11200` loggt Pfad/Kick/VU1-Start je TEX0 auf diese Textur). Ohne
        Flare: 12 statt 20 solcher Kicks, in drei Läufen (mit/ohne Mitschnitt) in denselben
        Halbbildern. `T5_COVERFIELD=1` (EE-Funktionen je Vsync → `coverfield.txt`): 0x252688
        (Flare-Element zeichnen), 0x252190, 0x2522E0, 0x22ED00 laufen genau in den Bildern mit
        vollem Flare (1943/1943).
        Effekt-Schleife `FUN_00252c70` → `FUN_00252b98` (pro Effekt): misst mit
        `FUN_002ddc00` die verstrichene Bildzeit in % (Timer 1 `RCNT1` seit Bildanfang
        `DAT_00154e80`, Takt 147 456 000 / fps) + Abschnittsdauer (`FUN_002ddcb0` Marken
        0xd − 0xc); ab **95 %** werden die restlichen Effekte des Bildes nicht mehr gezeichnet.
        Dazu Hysterese über `FUN_002dde30` (anderer Lastwert): ≥ 120 % → Effektbudget 0, < 100 %
        → wieder 480 (`DAT_00153260/64/68`).
        Der Abschalter war die Hysterese: der Lastwert ist die **Renderdauer** des letzten Bildes
        (0x440480 + 0x238 = Timer 1 beim Render-DMA-Ende, Handler 0x31d708 in 0x334e88, minus
        Startzeit). `T5_T1LOG=1` (jedes Lesen/Schreiben von Timer 1 → `t1log.txt`: Last in %,
        Aufrufe, Renderdauer, COUNT/MODE/Umläufe/COMP) zeigte: normal ~60 %, aber alle ~7 Bilder
        negativ (als ~174 000 % = 2^32) → Effekte aus.
        **Fehler in `timers.rs`:** Timer 1 läuft mit BUSCLK/256, CMPE + OVFE, COMP = 0. Der
        Umlauf 0xFFFF → 0 ist Compare UND Überlauf zugleich; `schedule` wählt nur eins
        (Compare) → OVFF nie gesetzt → der Handler 0x337718 zählte nie einen Umlauf
        (`DAT_00440a60` blieb 1), die Spieluhr sprang alle 2^16 Takte zurück. Fix: bei COMP = 0
        ohne ZRET setzt das Compare-Ereignis auch OVFF (+ Interrupt bei OVFE), wie PCSX2. Dazu:
        fällige Timer-Ereignisse vor jedem Timer-Zugriff verarbeiten und alle der Reihe nach
        (Neuplanung ab Ereigniszeit, nicht ab „jetzt“).
        Ergebnis Polar: Bilder mit reduziertem Flare 254 → 4 (ein echter Host-Hänger, Renderdauer
        124 %), LOGIC 300/5 s. Nicht der Logik-Takt: nur der Wert, den Timer 1 liefert.

## P4 Programme
- [~] B12 Devil Within, Tekken 1 Arcade History, Tekken 2 Arcade History, Tekken 3 Arcade History: Schwarzbild, hängt.
      - **Devil Within behoben (17.10.):** hing in libgcc `_fpmul_parts` (Soft-Float-Double):
        `multu` lieferte HI = 0, weil Ghidra HI als Teil-Varnode `Uaf04:4` des Produkts
        `Uaf00:8` schreibt und gen_rust.py dafür eine eigene 0-Variable anlegte. Betraf ALLE
        7 EE-Programme (~5000 mult/multu/mult1/madd). Headless: Titelbild „PRESS START“.
      - Arcade History: 1. owadpcm_driver (registriert RPC 0x07650001) und vdgcu_d liefen nie
        durch die IOP-Pipeline → jetzt aus TK5DATA3-Eintrag 7 rekompiliert (Basis ab 0x100000).
        2. tekken1/tekken2 haben gleichen Entry + crt0 → T2 lief mit T1-Code; Programm-ident
        jetzt eindeutig.
      - **Offen:** T1, T2 und T3 (alle headless geprüft) warten auf `sceSifBindRpc(0x80001300)` = Sonys Dbc_Manager (libpad2/
        libdbc-Controllerkette: dbcman + sio2d, Gerätetreiber vdgcu_d spricht SIO2 direkt über
        sio2man #51/#61/#62). Braucht HLE für dbcman (wie padman) bzw. SIO2-Pad-Emulation.
        Menüweg headless: MODE SELECT 7× DOWN, CROSS, [RIGHT=T2], CROSS, CROSS (Controls),
        CROSS (NO speichern), LEFT + CROSS.

- [x] B16 Devil Within: 30 fps und Zeitlupe (Nutzer 17.10.). **Behoben 17.10.: Nutzertest im
      Fenster flüssige 60 fps, keine Zeitlupe.**
      - Messung (headless, im Spiel, 17.10.): GAME FRAMES 150 / 5 s, MTVU-Worker ~3000 ms /
        5 s (= ~20 ms pro Bild, Grenze 16,7 ms → Spiel lässt jedes 2. Bild aus), davon VU1
        ~2700 ms bei ~125 Mio. Paaren = ~21 ns/Paar (Tekken 5 mit Shapes: 2–4 ns).
      - VU1-Mikrocode von Devil Within ist komplett im Sheet (`T5_VUDUMP`-Lauf: 0 neue Paare/
        Blöcke, nur VU0-Starts offen), keine Status-Leser. Aber 3654 von 4003 VU1-Blöcken
        haben keine Eintritts-Shapes (gesammelt nur Moonlit/Polar) → generischer Blockpfad.
      - Nächster Schritt: Shapes im Fenster sammeln (`set T5_VUSHAPES=1` vor `play.bat`,
        Devil Within einige Minuten spielen → `rust\vu_dumps\shapes.txt`), dann
        `gen_vu.py --collect-shapes`, `gen_vu.py`, neu bauen. Ob das allein für 60 reicht, ist
        nicht gemessen.
      - Gesammelt (Nutzer, Fenster, 17.10.): 177 Shapes, davon nur 8 neu → die von Devil
        Within genutzten Blöcke hatten fast alle schon Shapes; die Annahme „fehlende Shapes“
        erklärt die ~21 ns/Paar NICHT. Nutzertest der fps offen. Nächster Schritt, falls
        weiter 30: Worker-Profil (`T5_PROFILE` + `T5_PROFILE_MTVU`, gibt es nur in
        t5-headless → Nutzer fragen).
      - Worker-Profil im Fenster (Nutzer, 17.10., `T5_PROFILE=120` + `T5_PROFILE_MTVU=1`, jetzt
        auch in t5-bevy): GAME 90–146 / 5 s, Worker 3,4–4,5 s / 5 s belegt. Proben:
        `vu::pair_dyn_nf` 38 %, `run_inner` 8 %, `pair_dyn` 4 %, Warten 25 %, übersetzte
        Blöcke < 2 %. `pair_dyn_nf` kommt nur aus `vu::resolve`, wenn ein VU1-Paar NICHT in
        `sheets/vu_microcode.csv` steht → der Nutzer spielt Bereiche mit Mikrocode, den mein
        Headless-Lauf (nur der Anfang) nie geladen hat. Nächster Schritt: `T5_VUDUMP=1` im
        Fenster durch dieselben Bereiche → `gen_vu.py --collect rust/vu_dumps` → `gen_vu.py`.
        (Achtung: eine erste Messung war mit noch gesetztem `T5_VUSHAPES` verfälscht.)
      - `T5_VUDUMP` im Fenster lief mit ~2 fps (Nutzer konnte nicht alles abdecken): +654
        Paare, +971 Blöcke. Dabei gefunden: **Devil Within lädt dieselben VU1-Programme an
        wechselnde Adressen** (Overlays hinter dem residenten Programm ab ~0x4FE, je Bild anders
        gepackt). Der Blockschlüssel enthielt die pc → jede neue Lage = unübersetzter Block,
        Paar für Paar (656 Blöcke mit gleichem Inhalt an bis zu 32 Adressen).
      - **Fix (17.10.): Blöcke adressunabhängig** (microVU-Prinzip): Schlüssel = FNV über
        VU-Nummer + Wörter (`vu::block_key`, `gen_vu.py block_key`), übersetzte Blöcke nehmen
        die pc zur Laufzeit (`pair_lean(pc, …)`, `vuc.py Block.at`). Sheet-Zeilen gleichen
        Inhalts zusammengeführt (5022 → 4233).
      - **VU1-Code statisch aus dem Programm:** `gen_vu.py --collect-elf devil` liest die
        Sektion `.vutext` (0x28E1F0–0x2A979F, 14 006 Paare): alle Paare (+1346), Blöcke per
        linearem Durchlauf + statischen Sprungzielen (1211, +542), Eintritts-Shapes statisch
        verfolgt ("-" am Sektionsanfang und nach jedem E-Bit-Block, Ausgangs-Shape → Nachfolger,
        max. 4 je Block; +207). Laufzeit-Einsprünge „mitten im Block“ in alten Dumps waren nur
        Folge des Paar-für-Paar-Laufs.
      - Prüfung: vu-bench Moonlit 3000/3000 bitgenau (3,3–3,6 ns/Paar auf diesem Windows-PC).
        Nutzertest 17.10.: flüssige 60 fps.

## Selbst gefunden
- [x] S0 **Fehlende EE-Funktionen** 0x2EE660 / 0x2EE6A0 / 0x2EE6E0: virtuelle Methoden (Aufruf per
      `jalr` aus Vtables), die Ghidra nie disassembliert hat.
      - Zur Laufzeit „unknown call target“, ~870 000 Mal pro Lauf, auch aus Interrupt-Pfaden.
      - Sie liefern Wahrheitswerte (Bitmasken-Vergleich): Ohne sie stehen Menü- und
        Spielzustände auf Zufallswerten. Möglicher Auslöser für B3, B9, B12 und Ton.
      - Fix: `extracted/seeds/main.csv` → `run_ghidra.sh main` → Sheets/Rust neu.
      - Stand 17.10.: alle drei in `sheets/functions.csv`, übersetzt (`recomp/main/part_09`)
        und in `t5-recomp/src/table.rs`. Laufzeitprüfung („unknown call target“) steht aus.
- [ ] S1 T4-CLUT-Lookup ohne CSA (Referenz `clut[index]`; die Hardware addiert CSA).
- [ ] S2 MODE SELECT ohne „THEATER“ (Freischaltung aus dem Spielstand? gegen PCSX2 mit
      derselben Memory Card prüfen).
