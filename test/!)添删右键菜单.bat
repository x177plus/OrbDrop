@ECHO OFF
SETLOCAL ENABLEDELAYEDEXPANSION
(PUSHD "%~DP0") && (REG QUERY "HKU\S-1-5-19" >NUL 2>&1) || (
powershell -Command "Start-Process '%~sdpnx0' -Verb RunAs" && EXIT)

:MENU
CLS
ECHO.&ECHO 1、添加资源管理器右键菜单项
ECHO.&ECHO 2、移除资源管理器右键菜单项
ECHO.&ECHO.

IF EXIST "%SystemRoot%\System32\choice.exe" (
    CHOICE /C 12 /N /M "请输入数字 (1,2): "
    SET choice=!ERRORLEVEL!
) ELSE (
    SET /P choice="请输入数字 (1,2): "
    SET choice=!choice:~0,1!
)

IF "!choice!"=="1" GOTO AddMenu
IF "!choice!"=="2" GOTO RemoveMenu
GOTO MENU

:AddMenu
reg add "HKCU\Software\Scooter Software\Beyond Compare" /f /v "ExePath" /d "\"%~dp0BCompare.exe\"" >NUL
reg add "HKCU\SOFTWARE\Scooter Software\Beyond Compare 5" /f /v "ExePath" /d "\"%~dp0BCompare.exe\"" >NUL
reg add "HKCU\SOFTWARE\Scooter Software\Beyond Compare 5\BcShellEx" /f /v "SavedLeft" /d "\"%~dp0BCompare.exe\"" >NUL
reg add "HKLM\SOFTWARE\WOW6432Node\Scooter Software\Beyond Compare" /f /v "ExePath" /d "\"%~dp0BCompare.exe\"" >NUL
reg add "HKLM\SOFTWARE\WOW6432Node\Scooter Software\Beyond Compare 5" /f /v "ExePath" /d "\"%~dp0BCompare.exe\"" >NUL
reg add "HKLM\SOFTWARE\WOW6432Node\Scooter Software\Beyond Compare 5\BcShellEx" /f /v "SavedLeft" /d "\"%~dp0BCompare.exe\"" >NUL
reg add "HKLM\SOFTWARE\Classes\CLSID\{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}\InProcServer32" /f /ve /d "\"%~dp0BCShellEx64.dll\"" >NUL
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\BCompare.exe" /f /ve /d "\"%~dp0BCompare.exe\"" >NUL
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\BCompare.exe" /f /v "UseURL" /t REG_DWORD /d "1" >NUL
reg add "HKLM\SOFTWARE\Classes\.bcss" /f /ve /d "BeyondCompare.Snapshot" >NUL
reg add "HKLM\SOFTWARE\Classes\BeyondCompare.Snapshot" /f /ve /d "Beyond Compare Snapshot" >NUL
reg add "HKLM\SOFTWARE\Classes\BeyondCompare.Snapshot\DefaultIcon" /f /ve /d "\"%~dp0BCompare.exe,0\"" >NUL
reg add "HKLM\SOFTWARE\Classes\BeyondCompare.Snapshot\shell\open\command" /f /ve /d "\"%~dp0BCompare.exe\" \"%%1\"" >NUL
reg add "HKLM\SOFTWARE\Classes\.bcpkg" /f /ve /d "BeyondCompare.SettingsPackage" >NUL
reg add "HKLM\SOFTWARE\Classes\BeyondCompare.SettingsPackage" /f /ve /d "Beyond Compare Settings Package" >NUL
reg add "HKLM\SOFTWARE\Classes\BeyondCompare.SettingsPackage" /f /v "EditFlags" /t REG_DWORD /d "0x00100000" >NUL
reg add "HKLM\SOFTWARE\Classes\BeyondCompare.SettingsPackage\DefaultIcon" /f /ve /d "\"%~dp0BCompare.exe,0\"" >NUL
reg add "HKLM\SOFTWARE\Classes\BeyondCompare.SettingsPackage\shell\open\command" /f /ve /d "\"%~dp0BCompare.exe\" \"%%1\"" >NUL
reg add "HKLM\SOFTWARE\Classes\CLSID\{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}" /f /ve /d "CirrusShellEx" >NUL
reg add "HKLM\SOFTWARE\Classes\CLSID\{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}\InProcServer32" /f /ve /d "\"%~dp0BCShellEx64.dll\"" >NUL
reg add "HKLM\SOFTWARE\Classes\CLSID\{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}\InProcServer32" /f /v "ThreadingModel" /d "Apartment" >NUL
reg add "HKLM\SOFTWARE\Classes\*\shellex\ContextMenuHandlers\CirrusShellEx" /f /ve /d "{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}" >NUL
reg add "HKLM\SOFTWARE\Classes\Folder\shellex\ContextMenuHandlers\CirrusShellEx" /f /ve /d "{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}" >NUL
reg add "HKLM\SOFTWARE\Classes\lnkfile\shellex\ContextMenuHandlers\CirrusShellEx" /f /ve /d "{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}" >NUL
reg add "HKLM\SOFTWARE\Classes\Directory\shellex\ContextMenuHandlers\CirrusShellEx" /f /ve /d "{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}" >NUL
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved" /f /v "{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}" /d "Beyond Compare 5 Shell Extension" >NUL
reg add "HKLM\SYSTEM\CurrentControlSet\Services\EventLog\Application\Beyond Compare 5" /f /v "TypesSupported" /t REG_DWORD /d "7" >NUL 
reg add "HKLM\SYSTEM\CurrentControlSet\Services\EventLog\Application\Beyond Compare 5" /f /v "EventMessageFile" /d "\"%~dp0BCompare.exe\"" >NUL
ECHO.&ECHO 添加完成 
TIMEOUT /t 3 >NUL&CLS&GOTO MENU

:RemoveMenu
reg delete "HKLM\SOFTWARE\Classes\.bcss" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\Classes\.bcpkg" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\Classes\BeyondCompare.Snapshot" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\Classes\BeyondCompare.SettingsPackage" /F>NUL 2>NUL
reg delete "HKCU\SOFTWARE\Scooter Software\Beyond Compare" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\Scooter Software\Beyond Compare" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\Scooter Software\Beyond Compare 5" /F>NUL 2>NUL
reg delete "HKCU\SOFTWARE\Scooter Software\Beyond Compare 5" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\WOW6432Node\Scooter Software\Beyond Compare" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\WOW6432Node\Scooter Software\Beyond Compare 5" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\BCompare.exe" /F >NUL 2>NUL
reg delete "HKLM\SOFTWARE\Classes\CLSID\{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}" /F >NUL 2>NUL
reg delete "HKLM\SYSTEM\CurrentControlSet\Services\EventLog\Application\Beyond Compare 5" /F>NUL 2>NUL
reg delete "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved" /F /v "{812BC6B5-83CF-4AD9-97C1-6C60C8D025C5}">NUL 2>NUL

ECHO.&ECHO 移除完成
ECHO.&ECHO ghxi.com
TIMEOUT /t 5 >NUL&CLS&GOTO MENU