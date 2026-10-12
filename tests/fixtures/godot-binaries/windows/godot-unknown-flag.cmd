@echo off
rem Fake Godot probe fixture: godot-unknown-flag. Not a real Godot build.
if "%~1"=="--version" goto version
if "%~1"=="--help" goto helpscreen
exit /b 0
:version
echo 4.7.1.stable.official.b09f793f5
exit /b 0
:helpscreen
echo Unknown command line option: --help
echo Use --help to list all options.
exit /b 1
