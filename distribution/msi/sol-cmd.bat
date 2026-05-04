@echo off
echo *** Sol command prompt environment ***
echo.
echo Start Sol by running
echo     sol --config config\sol.toml
echo or use
echo     sol --help
echo to get help.
cd %~dp0
cmd /k set PATH=%~dp0bin;%PATH%
