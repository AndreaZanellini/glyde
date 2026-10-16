; SPEC §7's Windows installer (Inno Setup 6, preinstalled on GitHub's
; windows-latest runners). Built by the release workflow:
;   iscc /DAppVersion=<version> /DSourceExe=<path to glyde.exe> /DOutputDir=<dir> glyde.iss
; Unsigned until a code-signing certificate exists (SPEC §7); the release
; notes carry the SmartScreen workaround.

[Setup]
AppId={{6F2C3B1E-7A44-4C2B-9C0E-5D3A1B7E9F21}
AppName=Glyde
AppVersion={#AppVersion}
AppPublisher=The Glyde Authors
DefaultDirName={autopf}\Glyde
DefaultGroupName=Glyde
OutputDir={#OutputDir}
OutputBaseFilename=glyde-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequiredOverridesAllowed=dialog
LicenseFile=..\..\LICENSE

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "glyde.exe"
Source: "..\..\LICENSE"; DestDir: "{app}"
Source: "..\..\NOTICE"; DestDir: "{app}"

[Icons]
Name: "{group}\Glyde"; Filename: "{app}\glyde.exe"
Name: "{autodesktop}\Glyde"; Filename: "{app}\glyde.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Run]
Filename: "{app}\glyde.exe"; Description: "Launch Glyde"; Flags: nowait postinstall skipifsilent
