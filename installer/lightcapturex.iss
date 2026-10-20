; LightCaptureX インストーラー（Inno Setup 6）
; ビルド: ISCC.exe /DAppVersion=0.2.1 lightcapturex.iss
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{CD69D7CD-DD1D-403A-AEAB-95D40D98C76B}
AppName=LightCaptureX
AppVersion={#AppVersion}
AppPublisher=Lapius7
AppPublisherURL=https://github.com/Lapius7/lightcapturex
DefaultDirName={localappdata}\Programs\LightCaptureX
DefaultGroupName=LightCaptureX
PrivilegesRequired=lowest
OutputDir=Output
OutputBaseFilename=lightcapturex-installer-v{#AppVersion}

Compression=lzma2/ultra
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\lightcapturex.exe
AppMutex=Local\LightCaptureX.single
CloseApplications=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "..\target\x86_64-pc-windows-gnu\release\lightcapturex.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\LightCaptureX"; Filename: "{app}\lightcapturex.exe"
Name: "{autodesktop}\LightCaptureX"; Filename: "{app}\lightcapturex.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\lightcapturex.exe"; Description: "{cm:LaunchProgram,LightCaptureX}"; Flags: nowait postinstall skipifsilent

; 自動アップデートが残す一時ファイル
[UninstallDelete]
Type: files; Name: "{app}\lightcapturex.exe.old"
Type: files; Name: "{app}\lightcapturex.exe.new"
