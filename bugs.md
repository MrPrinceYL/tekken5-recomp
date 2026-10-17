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
- [ ] B11 Polar Paradise (Pinguin-Stage): Logik 60, aber nur 30 gerenderte Bilder/s.

## P4 Programme
- [ ] B12 Devil Within: Schwarzbild, hängt.

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
