# Plan: 60 Logik-Ticks/s in jeder Stage, mit einem GPU-Rasterizer

Stand 14.10.2026. Entscheidung des Nutzers: Der GS wird per Compute-Shader bitgenau auf der
Grafikkarte gezeichnet (Prinzip paraLLEl-GS). Das Spiel bleibt 1:1 rekompiliert; nur die
Hardware-Schnittstelle „GS“ wechselt von CPU auf GPU.

## 1. Befund (gemessen)

| Moonlit Wilderness (s6), pro Logik-Tick | heute | Rasterisierung kostenlos (`T5_NODRAW=1`) |
|---|---|---|
| LOGIC FRAMES / 5 s | 221 | **300** |
| Ticks > 1 Halbbild | 152 von 666 | **0** von 898 |
| EE / VU-Worker / GS-Thread | 11,9 / 17,4 / 9,6 ms | 8,6 / 12,7 / 8,6 ms |

- **Allein das Rastern der Pixel verhindert die 60.** EE, VU1 (statisch übersetzt), DMA und
  der GS-Thread ohne Zeichnen bleiben unter 16,7 ms.
- Auf der CPU (13 Threads, ~56 ns/px) ist das nicht mehr lösbar. Die Stages zeichnen bis zu
  ~4 Mpx pro Bild mit Blending, Z und Bilinear.
- Mit `T5_NODRAW=1` (nur Messung, falsches Bild) lässt sich jede Stage so gegenprüfen.

### Hindernis für Bitgenauigkeit auf der GPU
Der CPU-Rasterizer (`raster.rs`) rechnet so:
- Baryzentrische Gewichte in `f64`.
- Attribute werden pro Pixel inkrementell in `f32` aufsummiert. Das Ergebnis hängt vom
  Spananfang ab.
- STQ-Division in `f32`, LOD über `log2` in `f32`.

Die GPU kennt kein `f64`. Ihre `f32`-Division ist nicht korrekt gerundet, und der Treiber darf
FMA zusammenziehen. **Diese Rechenwege sind auf der GPU nicht reproduzierbar.**

Lösung wie die echte Hardware: Der GS selbst interpoliert in Festkomma. Ich definiere die
Rastersemantik **vollständig ganzzahlig** und pro Pixel geschlossen auswertbar. CPU und GPU
führen dieselbe Ganzzahl-Arithmetik aus und sind damit bitgleich. Die CPU-Fassung bleibt
Referenz und Rückfall.

## 2. Architektur

```
EE ─▶ VU-Worker (VIF1/VU1/GIF) ─▶ GS-Thread ──────────────▶ GPU (wgpu, Compute)
                                  Register, Setup,           VRAM = Storage-Buffer 4 MB
                                  Batches, Uploads,          ├ upload.wgsl (Swizzle-Schreiben)
                                  CLUT, Anzeige              ├ clut.wgsl   (CSA/CLD)
                                                             ├ bin.wgsl    (Prims → Kacheln)
                                                             ├ raster.wgsl (Kachel, Prims in
                                                             │              Reihenfolge)
                                                             └ readout.wgsl (PMODE/Merge → RGBA)
```

- Die GPU besitzt den GS-Speicher. Die CPU liest ihn nur bei Local→Host-Transfers (selten,
  synchron) und für die Anzeige zurück. Die Anzeige wird asynchron ausgelesen; das Frontend ist
  ohnehin ein Halbbild versetzt.
- Der GS-Thread zeichnet nichts mehr. Er hängt pro Batch Befehle an einen Encoder an. Abgeschickt
  wird bei Vsync, bei einer Download-Sync oder ab N Befehlen.
- Reihenfolge: Innerhalb eines Batches zeichnet jede Kachel ihre Primitive in GS-Reihenfolge.
  Zwischen Batches, Uploads und CLUT-Loads sorgen die Dispatch-Grenzen der Queue für die
  Reihenfolge.
- Gefahren im selben Batch: Ein Primitiv sampelt Seiten, die der Batch schreibt, oder FB und Z
  überlappen über Kachelgrenzen. Dann wird der Batch geteilt bzw. seriell gezeichnet (eine
  Workgroup). Die vorhandene Seitenverfolgung (`pages`/`reads`) liefert das.
- Die Textur wird direkt aus dem VRAM-Puffer gesampelt (Swizzle-Formeln aus `gs.rs`), die
  Palette aus dem CLUT-Puffer. Texturcache und Dekodier-Aufträge entfallen.
- Ohne Ereignisrückkanal: SIGNAL, FINISH und LABEL bleiben im `GsFront` (EE-Seite) und hängen
  nicht vom Zeichnen ab. Das Spiel wartet nur auf die DMA (s19) und den Vsync (s18).

## 3. Schritte

Jeder Schritt wird so abgenommen:
1. `cargo build` fehlerfrei.
2. Parität per `gs-bench` (VRAM-Hash pro Halbbild). Mitschnitte aus 4 Stages: s0, s6, s7, s12,
   außerdem Menü und Charakterauswahl.
3. Messung `stage.sh`.
4. Eintrag in CLAUDE.md.

### Phase A: Ganzzahlige Referenzsemantik auf der CPU ✅ (14.10.)
- [x] A1 Genutzte Zustände (Moonlit, `T5_STATELOG`):
      - Texturen: T8, T4, CT32, CT24; Bilinear; Mipmaps mit Q-abhängigem LOD (MXL 1–2, MMIN 4)
        und LCM.
      - Alle Wrap-Modi; Fog, Alphatest (alle Modi), Z24/Z16S, FBMSK, Linien.
      - Nicht genutzt: Dither, PABE, FBA. COLCLAMP ist immer 1.
- [x] A2–A5 `raster.rs`:
      - `Plane` (c0 + cx·dx + cy·dy, Wrapping-`i64`, 16 Nachkommabits, Ursprung =
        Primitiv-Bounding-Box) und `Interp` (z, rgba, fog, s, t, q).
      - Koeffizienten exakt per `i128` (nur CPU-Setup).
      - STQ mit gemeinsamem Exponenten; u16 = ⌊S·16·2^tw / Q⌋.
      - LOD per `log2_16` (Tabelle mit 256 Einträgen) in 1/16.
      - Linien als Ganzzahl-DDA.
      - Schnellpfade `sprite_px`/`tri_px` entfernt: eine einzige Semantik in `plot_g`.
- [x] A6 Abnahme:
      - Optisch gleich zum alten Stand. Abweichungen nur an Alphatest-Kanten und Texelgrenzen
        (andere Rundung der Texelwahl).
      - Seriell = parallel. Referenz-Hash des Moonlit-Mitschnitts: **c6db9cd3b6018142**.
      - CPU-Weg ~40 % langsamer (14,9 statt 10,6 ms/Halbbild im Bench), egal, sobald die GPU
        zeichnet.

### Phase B–D: GPU-Rasterizer ✅ (14.10.)
- [x] B0 STQ-Division in Wrapping-`i64` (`div_floor64`), gleich auf CPU und GPU.
- [x] B1 `wgpu` 29 in `ps2-runtime` (`gpu.rs`).
      - Vulkan wird erzwungen: DX12 bietet auf der RTX 4070 SUPER kein `SHADER_INT64`.
      - Test `gpu::tests::int64_matches_cpu`: Mul, Shift, Add und Floor-Division in `i64`
        bitgleich.
      - Gefunden: `%` auf `i64` ist im Shader falsch. Ersatz: r = n − q·d.
- [x] Gemeinsames Setup `raster::prim_setup` → `Prim` (Kanten, Ebenen, Bounding-Box).
      - Gilt für CPU-Referenz (`draw_prim`) und GPU (`gpu_prim`).
      - Koeffizienten per `f64`: deterministisch, nur die CPU rechnet sie.
      - Linien und Punkte jetzt mit Scissor (Hardware-Verhalten, vorher ohne).
- [x] `gs.wgsl` (Raster / Raster seriell / Upload / Local→Local / CLUT) als 1:1-Übertragung.
      - Übertragen sind `plot_g`, `sample`, `texel` und `read_pixel`/`store_pixel`.
      - Swizzle-Tabellen werden aus `gs::addr*` erzeugt.
      - VRAM ist `array<atomic<u32>>`, Teilwort-Schreiben per `atomicAnd`/`atomicOr`.
- [x] `GsGpu`: Arenen pro Submit (States, Prims, Kacheln, Listen, Upload-Daten, Parameter mit
      dynamischem Offset), CPU-Binning in 16×16-Kacheln, CLUT-Ring (4096 Slots).
- [x] Einbau (`T5_GPU=1`):
      - Batches, Uploads, Local→Local und CLUT-Loads gehen in den GPU-Strom.
      - Local→Host liest synchron zurück.
      - Anzeige: asynchroner Readback (Ring aus 3 Staging-Puffern, Thread „gs readout“, zählt
        das Halbbild dort fertig).
      - `gs-bench` liest synchron zurück (Paritätsprüfung).
- [x] GS-Thread entlastet:
      - Setup parallel (rayon).
      - `attr_vals` ohne Allokation.
      - Upload-Position in geschlossener Form.
      - Keine Uhr pro Primitiv.
      - Local→Local-Überlappung über Adressbereiche.
- [x] **Parität: Moonlit-Mitschnitt GPU = CPU in allen 121 Halbbildern (c6db9cd3b6018142).**
- [x] **Moonlit Wilderness: 300 299 300 300 300 LOGIC / 5 s, alle Ticks 1 Halbbild** (vorher 221).

### Phase E: Abnahme ✅ (14.10.)
- [x] E1 Alle 22 Stages (`stage.sh 0..21`, `T5_NOCOUNT=1`, GPU):
      - LOGIC FRAMES 297–300 pro 5 s in **jeder** Stage, Ziel ≥ 295 erreicht.
      - Vorher: s6 185, s7 215, s12 215 usw.
      - Ergebnisse in `run/allstages_gpu.txt`.
- [x] E2 Parität CPU ↔ GPU im Moonlit-Mitschnitt: 121/121 Halbbilder identisch.
- [x] GPU ist Standard. `T5_GPU=0` = CPU-Referenz, z. B. für `gs-bench`-Vergleiche. Ohne
      geeignete GPU zeichnet automatisch die CPU.
- [ ] E3 Test durch den Nutzer über `play.bat`.
- [ ] Offen:
      - Mitschnitte weiterer Stages und Menüs gegen die CPU-Referenz prüfen.
      - Stagenamen den Indizes zuordnen.
      - Boss-Stages.
      - T4-CLUT-Lookup ohne CSA prüfen (Referenz liest `clut[index]`, die Hardware nutzt CSA).

## 4. Risiken und Gegenmaßnahmen

| Risiko | Gegenmaßnahme |
|---|---|
| GPU-Arithmetik weicht ab | Nur Ganzzahl, keine `f32` im Shader-Datenpfad; Parität pro Halbbild |
| Viele kleine Batches → Dispatch-Overhead | Batches pro Encoder sammeln, ein Submit pro Vsync; kleine Batches zusammenfassen, solange keine Gefahr besteht |
| Downloads (Local→Host) stoppen die Pipeline | Zählen (`T5_FLUSHSTATS`); im Kampf erwartet selten |
| Ausgaberaten-Latenz | Anzeige asynchron, 1 Halbbild Versatz, betrifft die Logik nicht |
| Rückkopplung (Textur = eigener FB) | Batch teilen, im Extremfall seriell (eine Workgroup) |

## 5. Lehren
- Erst die Obergrenze messen (`T5_NODRAW`), dann bauen.
- Jede Synchronisationsstruktur mitprofilieren: Die Listensuche in den Raster-Aufträgen kostete
  9 ms pro Tick. Sie ist jetzt eine Tabelle pro Seite.
- Parität vor Geschwindigkeit: Der Moonlit-Bildfehler war ein Wettlauf (Dekodieren vor dem
  CLUT-Auftrag), behoben.
