#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif

[Setup]
AppId={{9376D52E-A217-4F16-8B35-263352052A61}
AppName=EchoBridge
AppVersion={#AppVersion}
AppPublisher=EchoBridge
DefaultDirName={localappdata}\Programs\EchoBridge
DefaultGroupName=EchoBridge
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
WizardStyle=modern
OutputDir=..\dist
OutputBaseFilename=EchoBridge-Setup
SetupIconFile=..\crates\app\assets\icon.ico
UninstallDisplayIcon={app}\EchoBridge.exe
Compression=lzma2
SolidCompression=yes
CloseApplications=yes
RestartApplications=no
AppMutex=Local\EchoBridge.App

[Tasks]
Name: "startup"; Description: "Run EchoBridge at Windows sign-in"; GroupDescription: "Startup:"; Flags: checkedonce
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"

[Files]
Source: "..\dist\EchoBridge.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\START-HERE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "EchoBridge"; ValueData: """{app}\EchoBridge.exe"" --background --autostart"; Tasks: startup; Flags: uninsdeletevalue

[Icons]
Name: "{autoprograms}\EchoBridge"; Filename: "{app}\EchoBridge.exe"
Name: "{autodesktop}\EchoBridge"; Filename: "{app}\EchoBridge.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\EchoBridge.exe"; Description: "Open EchoBridge"; Flags: nowait postinstall skipifsilent
