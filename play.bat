@echo off
rem Launch the recompiled Tekken 5 (builds first if needed; the first build takes a while).
rem The disc image defaults to E:\Decompilation\Game\Tekken 5 (USA).iso; set T5_DISC to use
rem another one. Memory cards are folders: memcard\port0 and memcard\port1 next to this file.
rem (No parenthesized blocks below: the ")" in "(USA)" would end them early.)
setlocal
if not defined T5_DISC set "T5_DISC=E:\Decompilation\Game\Tekken 5 (USA).iso"
if exist "%T5_DISC%" goto disc_ok
echo Disc image not found: "%T5_DISC%"
echo Set T5_DISC to the path of the Tekken 5 .iso file.
pause
exit /b 1
:disc_ok
if not defined T5_MEMCARD set "T5_MEMCARD=%~dp0memcard"
rem The last session's sound is recorded to last_session.wav (for checking sound bugs).
if not defined T5_WAVDUMP set "T5_WAVDUMP=%~dp0last_session.wav"
cd /d "%~dp0"
if exist "rust\Cargo.toml" goto generated
echo Generating the Rust code from the sheets...
python tools\gen_rust.py
if errorlevel 1 goto failed
:generated
cd /d "%~dp0rust"
cargo run -p t5-bevy %*
if errorlevel 1 goto failed
exit /b 0
:failed
pause
exit /b 1
