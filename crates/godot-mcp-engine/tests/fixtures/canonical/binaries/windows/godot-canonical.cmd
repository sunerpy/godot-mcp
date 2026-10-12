@echo off
rem A fake Godot 4.4+ editor build that runs the bundled canonical-write helper.
rem
rem Behavioural twin of the sibling `unix/godot-canonical.sh`; see that file for
rem the full contract, the exit-code table and the reasoning. This variant exists
rem so the Todo 17 acceptance suite has a fixture on Windows too.
rem
rem NOT VERIFIED ON WINDOWS: this repository's host is Linux and has no Windows
rem runner, so only the `.sh` variant has actually been executed.
setlocal enabledelayedexpansion

set "project="
set "script="
set "request="
set "response="
set "saw_separator=0"

:parse
if "%~1"=="" goto parsed
if "!saw_separator!"=="1" (
  if /I "%~1"=="--request" (
    set "request=%~2"
    shift
    shift
    goto parse
  )
  if /I "%~1"=="--response" (
    set "response=%~2"
    shift
    shift
    goto parse
  )
  shift
  goto parse
)
if "%~1"=="--" (
  set "saw_separator=1"
  shift
  goto parse
)
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

set "mode=ok"
set "uid=uid://bfakeengine001"
if exist "%conf%" (
  for /f "tokens=1,* delims==" %%A in ('type "%conf%"') do (
    if /I "%%A"=="helper" set "mode=%%B"
    if /I "%%A"=="uid" set "uid=%%B"
  )
)

rem The silent no-op an export-template build performs: the flag is ignored and
rem the process exits 0 having done nothing at all.
if /I "!mode!"=="silent-ignore" exit /b 0
if "%script%"=="" exit /b 0

if "%request%"=="" (
  echo canonical_write: --request and --response are both required 1>&2
  exit /b 64
)
if "%response%"=="" (
  echo canonical_write: --request and --response are both required 1>&2
  exit /b 64
)

set "request_file=%project%\%request%"
set "response_file=%project%\%response%"
if not exist "%request_file%" (
  echo canonical_write: unreadable request %request% 1>&2
  exit /b 64
)

set "operation="
set "target="
set "artifact="
set "resource_name="
set "root_type="
set "root_name="
for /f "tokens=1,* delims==" %%A in ('type "%request_file%"') do (
  if /I "%%A"=="operation" set "operation=%%B"
  if /I "%%A"=="target" set "target=%%B"
  if /I "%%A"=="artifact" set "artifact=%%B"
  if /I "%%A"=="resource_name" set "resource_name=%%B"
  if /I "%%A"=="root_type" set "root_type=%%B"
  if /I "%%A"=="root_name" set "root_name=%%B"
)

echo !artifact! | findstr /b /c:"godot_mcp_helpers/2/staging/" >nul
if errorlevel 1 (
  call :refuse artifact_outside_staging
  exit /b 65
)

set "extension="
for %%E in ("!target!") do set "extension=%%~xE"
set "extension=!extension:.=!"

if /I "!extension!"=="scn" goto binary
if /I "!extension!"=="res" goto binary
if /I "!extension!"=="tscn" goto text_ok
if /I "!extension!"=="tres" goto text_ok
call :refuse not_a_text_resource
exit /b 65

:binary
> "%response_file%" (
  echo helper_version=2
  echo reason=binary_resource
  echo status=refused
  echo target=res://!target!
)
exit /b 65

:text_ok
if /I "!operation!"=="create_resource" (
  if exist "%project%\!target!" (
    call :refuse already_exists
    exit /b 65
  )
) else (
  if not exist "%project%\!target!" (
    call :refuse target_missing
    exit /b 65
  )
)

if /I "!mode!"=="save-failed" (
  > "%response_file%" (
    echo helper_version=2
    echo reason=save_failed
    echo status=failed
  )
  exit /b 70
)

echo !mode! | findstr /b /c:"uid-conflict:" >nul
if not errorlevel 1 (
  set "existing=!mode:uid-conflict:=!"
  > "%response_file%" (
    echo existing_path=res://!existing!
    echo helper_version=2
    echo reason=uid_conflict
    echo requested_path=res://!target!
    echo status=refused
    echo uid=!uid!
  )
  exit /b 65
)

set "artifact_file=%project%\!artifact!"
for %%D in ("!artifact_file!") do if not exist "%%~dpD" mkdir "%%~dpD"

if /I "!extension!"=="tres" (
  > "!artifact_file!" (
    echo [gd_resource type="Resource" format=3 uid="!uid!"]
    echo.
    echo [resource]
    if not "!resource_name!"=="" echo resource_name = "!resource_name!"
  )
) else (
  if "!root_type!"=="" set "root_type=Node"
  if "!root_name!"=="" set "root_name=Root"
  > "!artifact_file!" (
    echo [gd_scene load_steps=1 format=3 uid="!uid!"]
    echo.
    echo [node name="!root_name!" type="!root_type!"]
  )
)

if /I "!mode!"=="no-response" exit /b 0

> "%response_file%" (
  echo artifact=res://!artifact!
  echo helper_version=2
  echo status=ok
  echo target=res://!target!
  echo uid=!uid!
)
exit /b 0

:refuse
> "%response_file%" (
  echo helper_version=2
  echo reason=%~1
  echo status=refused
)
exit /b 0
