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
- [ ] B15 Stick-Bewegungen werden vom Spiel nicht erkannt (Nutzer 17.10., unter Linux
      getestet). B1-Fix (`Input::sticks`) prüfen: kommen Achswerte über gilrs/HID an?

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
      (Pool-Stage, Polar Paradise, ...). Vermutlich gemeinsame Ursache (Alpha-Blend/Partikel-Pfad im GS).
      Analyse Sun Flare 17.10. (Linux, Polar = `stage.sh 10`, `T5_BURST=75` = jedes Bild 1 s lang,
      Ergebnisse in `run/flare/`):
      - Reproduziert headless: in einzelnen Bildern fehlt der GANZE Flare (Strahlen + Kreise),
        nicht nur blasser. Messung: Helligkeit oberes linkes Bilddrittel 150,4/150,8 normal,
        145,6/145,9 ohne Flare.
      - GPU-Raster: 5 von 61 Bildern ohne Flare. CPU-Raster (`T5_GPU=0`, langsamer): ~60 % ohne,
        oft viele Bilder am Stück. → kein GPU-Fehler; hängt von der Laufzeit/Last ab.
      - Kein Verdeckungstest per Rücklesen: `T5_DLLOG=1` (protokolliert jede GS→EE-Übertragung,
        `gs.rs local_to_host`) zeigt im ganzen Lauf 0 Rücklesungen.
      - Nächste Vermutung: das Spiel misst die Render-Last (Timer / FINISH / SIGNAL / Vsync-Abstand)
        und lässt den Flare bei „zu langsam“ weg. Gerätezeit = Host-Zeit (`Machine::now`), der
        MTVU-Worker / GS-Thread liefern FINISH/SIGNAL u. U. später als echte HW. Nächster Schritt:
        in der Flare-Zeichenfunktion die Bedingung finden (GS-Mitschnitt `T5_GSCAPTURE`: fehlen
        die Flare-Primitive im Stream? dann EE-Entscheidung; RAM-Diff mit `T5_RAMSNAP`).

## P4 Programme
- [ ] B12 Devil Within, Tekken 1 Arcade History, Tekken 2 Arcade History, Tekken 3 Arcade History: Schwarzbild, hängt.

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
