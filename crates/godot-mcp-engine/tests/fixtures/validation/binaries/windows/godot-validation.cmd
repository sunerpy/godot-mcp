@echo off
rem A fake Godot 4.4+ editor build, for validation / import / test execution.
rem
rem Byte-for-byte behavioural twin of the sibling `unix/godot-validation.sh`;
rem see that file for the full contract and the engine-source citations. This
rem variant exists so the acceptance suite has a fixture on Windows too.
rem
rem NOT VERIFIED ON WINDOWS: this repository's CI host is Linux and has no
rem Windows runner, so only the `.sh` variant has actually been executed.
setlocal enabledelayedexpansion

set "project="
set "script="
set "check_only=0"
set "do_import=0"
set "run_script="

:parse
if "%~1"=="" goto parsed
if /I "%~1"=="--path" (
  set "project=%~2"
  shift
  shift
  goto parse
)
if /I "%~1"=="--script" (
  set "script=%~2"
  shift
  shift
  goto parse
)
if /I "%~1"=="-s" (
  set "run_script=%~2"
  shift
  shift
  goto parse
)
if /I "%~1"=="--check-only" (
  set "check_only=1"
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

if "%check_only%%script%"=="1" goto after_check
if "%check_only%"=="1" if not "%script%"=="" (
  set "resolved=%script:res://=%"
  set "resolved=%project%\%resolved:/=\%"
  if not exist "!resolved!" (
    echo ERROR: Can't load script: %script% 1>&2
    echo    at: main ^(main/main.cpp:3927^) 1>&2
    exit /b 1
  )
  set "errline="
  for /f "tokens=1 delims=:" %%L in ('findstr /N "godot_mcp_calibration_syntax_error" "!resolved!"') do (
    if "!errline!"=="" set "errline=%%L"
  )
  if not "!errline!"=="" (
    echo SCRIPT ERROR: Parse Error: Expected statement, found "Identifier" instead. 1>&2
    echo    at: GDScript::reload ^(%script%:!errline!^) 1>&2
    set "failure_exit=1"
    if exist "%conf%" (
      for /f "tokens=1,* delims==" %%A in ('type "%conf%"') do (
        if /I "%%A"=="check_failure_exit" set "failure_exit=%%B"
      )
    )
    exit /b !failure_exit!
  )
  rem A valid script: the engine prints nothing whatsoever.
  exit /b 0
)

:after_check
if "%do_import%"=="1" (
  set "import_mode=ok"
  if exist "%conf%" (
    for /f "tokens=1,* delims==" %%A in ('type "%conf%"') do (
      if /I "%%A"=="import" set "import_mode=%%B"
    )
  )
  echo !import_mode! | findstr /B "fail:" >nul
  if not errorlevel 1 (
    set "resource=!import_mode:fail:=!"
    echo ERROR: Failed to import resource: !resource! 1>&2
    echo    at: _reimport_file ^(editor/editor_file_system.cpp:2371^) 1>&2
    exit /b 1
  )
  if not exist "%project%\.godot" mkdir "%project%\.godot"
  echo godot-mcp-fake uid cache>"%project%\.godot\uid_cache.bin"
  echo Godot Engine v4.7.1.stable.official - https://godotengine.org
  echo Import: begin
  echo Import: end
  exit /b 0
)

if not "%run_script%"=="" (
  set "test_output="
  set "test_exit=0"
  set "test_sleep_ms=0"
  if exist "%conf%" (
    for /f "tokens=1,* delims==" %%A in ('type "%conf%"') do (
      if /I "%%A"=="test_output" set "test_output=%%B"
      if /I "%%A"=="test_exit" set "test_exit=%%B"
      if /I "%%A"=="test_sleep_ms" set "test_sleep_ms=%%B"
    )
  )
  if not "!test_sleep_ms!"=="0" (
    rem Outlive a short deadline deliberately.
    ping -n 10 127.0.0.1 >nul
  )
  if not "!test_output!"=="" (
    set "out_rel=!test_output:/=\!"
    if exist "%project%\!out_rel!" type "%project%\!out_rel!"
  )
  exit /b !test_exit!
)

rem Nothing recognisable was requested.
exit /b 0
