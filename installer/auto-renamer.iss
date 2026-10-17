#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif

[Setup]
AppId={{7C4E9A21-5B38-4F6D-A1C0-8E2D47B9F013}
AppName=Auto Renamer
AppVersion={#AppVersion}
AppPublisher=BenZoN
DefaultDirName={localappdata}\Auto Renamer
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
LicenseFile=..\LICENSE
OutputDir=..\target\installer
OutputBaseFilename=AutoRenamer-Setup
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\auto-renamer.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
CloseApplications=yes
RestartApplications=no

[Files]
Source: "..\target\release\auto-renamer.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Auto Renamer"; Filename: "{app}\auto-renamer.exe"

[Run]
Filename: "{app}\auto-renamer.exe"; Description: "Start Auto Renamer"; Flags: nowait postinstall skipifsilent
