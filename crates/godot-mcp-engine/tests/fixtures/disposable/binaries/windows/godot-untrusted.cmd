@echo off
rem A fake Godot 4.4+ editor build for Todo 16's disposable-execution suite.
rem
rem Behavioural twin of the sibling `unix/godot-untrusted.sh`; see that file for
rem the full contract and the reasoning. This variant exists so the acceptance
rem suite has a fixture on Windows too.
rem
rem NOT VERIFIED ON WINDOWS: this repository's host is Linux and has no Windows
rem runner, so only the `.sh` variant has actually been executed.
setlocal enabledelayedexpansion

set "project="
set "do_import=0"

:parse
if "%~1"=="" goto parsed
if /I "%~1"=="--path" (
  set "project=%~2"
  shift
  shift
  goto parse
)
if /I "%~1"=="--import" (
  set "do_import=1"
  shift
  goto parse
)
if /I "%~1"=="--version" (
  echo 4.7.1.stable.official.b09f793f5
  exit /b 0
)
rem Unknown or irrelevant option: ignored without a diagnostic.
shift
goto parse

:parsed
if "%project%"=="" set "project=%CD%"
set "conf=%project%\fake-engine.conf"

if not "%do_import%"=="1" exit /b 0

set "import_sleep_ms=0"
if exist "%conf%" (
  for /f "tokens=1,* delims==" %%A in ('type "%conf%"') do (
    if /I "%%A"=="import_sleep_ms" set "import_sleep_ms=%%B"
  )
)
if not "!import_sleep_ms!"=="0" (
  rem Outlive a short deadline deliberately.
  ping -n 10 127.0.0.1 >nul
)

echo Godot Engine v4.7.1.stable.official - https://godotengine.org
echo Import: begin

if not exist "%project%\.godot" mkdir "%project%\.godot"
echo godot-mcp-fake uid cache>"%project%\.godot\uid_cache.bin"

rem An in-place source mutation.
echo.>>"%project%\project.godot"
echo ; mutated by the untrusted fixture during import>>"%project%\project.godot"

rem A destructive source mutation.
if exist "%project%\main.tscn" del /q "%project%\main.tscn"

rem An escape attempt one level above the project root.
echo godot-mcp fixture: the untrusted run reached this directory>"%project%\..\godot-mcp-escape-marker.txt"

rem Everything excluded that the run can nonetheless read.
set "saw=%project%\godot-mcp-saw-excluded.txt"
break>"%saw%"
for %%C in (.env .env.production private.pem credentials.json export_presets.cfg) do (
  if exist "%project%\%%C" echo %%C>>"%saw%"
)
if exist "%project%\.git\config" echo .git/config>>"%saw%"
if exist "%project%\.git\HEAD" echo .git/HEAD>>"%saw%"
if exist "%project%\.godot\project.godot" echo .godot/project.godot>>"%saw%"

echo Import: end
exit /b 0
