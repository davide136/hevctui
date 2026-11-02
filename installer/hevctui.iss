; Installer per-utente di hevctui (nessun diritto di amministratore).
; Compilato da scripts\release.ps1: iscc /DAppVersion=x.y.z /DSourceExe=<percorso hevctui.exe> installer\hevctui.iss
; L'updater interno lo lancia con /VERYSILENT: attende la chiusura dell'app e la rilancia a fine installazione.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #error "SourceExe non definito: passare /DSourceExe=<percorso hevctui.exe>"
#endif

[Setup]
AppId={{D750EA9E-6ECC-4329-B158-CFF1A2A0C656}
AppName=hevctui
AppVersion={#AppVersion}
AppVerName=hevctui {#AppVersion}
AppPublisher=davide136
AppPublisherURL=https://github.com/davide136/hevctui
AppUpdatesURL=https://github.com/davide136/hevctui/releases
DefaultDirName={autopf}\hevctui
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=hevctui-setup-{#AppVersion}
SetupIconFile=..\assets\hevctui.ico
UninstallDisplayIcon={app}\hevctui.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; la chiusura dell'app è gestita in InitializeSetup tramite il mutex
CloseApplications=no

[Languages]
Name: "it"; MessagesFile: "compiler:Languages\Italian.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\hevctui.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\hevctui"; Filename: "{app}\hevctui.exe"; IconFilename: "{app}\hevctui.ico"
Name: "{autodesktop}\hevctui"; Filename: "{app}\hevctui.exe"; IconFilename: "{app}\hevctui.ico"; Tasks: desktopicon

[Run]
; senza skipifsilent: dopo un aggiornamento silenzioso l'app viene rilanciata
Filename: "{app}\hevctui.exe"; Description: "{cm:LaunchProgram,hevctui}"; Flags: nowait postinstall

[Code]
const
  RunningMutex = 'hevctui-running';

function InitializeSetup(): Boolean;
var
  i: Integer;
begin
  Result := True;
  if WizardSilent then begin
    // aggiornamento dall'app: le lascia il tempo di chiudersi (salvataggio indice, stop ffmpeg)
    i := 0;
    while CheckForMutexes(RunningMutex) and (i < 120) do begin
      Sleep(500);
      i := i + 1;
    end;
    Result := not CheckForMutexes(RunningMutex);
  end else begin
    while Result and CheckForMutexes(RunningMutex) do
      Result := MsgBox('hevctui è in esecuzione. Chiudila e premi Riprova per continuare.', mbError, MB_RETRYCANCEL) = IDRETRY;
  end;
end;
