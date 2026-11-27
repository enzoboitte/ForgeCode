@echo off
where py >nul 2>nul
if %errorlevel%==0 (
  py -3 -u "%~dp0conan_module.py" %*
) else (
  python -u "%~dp0conan_module.py" %*
)
