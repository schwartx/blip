; blip installer — Inno Setup 6
;
; Per-user by design. blip needs no elevation: it installs two exes under
; %LOCALAPPDATA%, keeps its config in %APPDATA%, and touches exactly one
; registry value it already manages itself. Requiring admin for that would be
; theatre, and it would break the one thing that actually matters — the panel
; must run inside the user's interactive session to be able to draw at all.
;
; Build:  iscc installer\blip.iss            (or: powershell .\build-installer.ps1)

#define AppName    "blip"
#define AppExe     "blipd.exe"
#define CliExe     "blip.exe"
; build-installer.ps1 passes /DAppVersion=<Cargo.toml version> so the version
; lives in exactly one place. The fallback is only for a bare `iscc blip.iss`.
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
AppId={{7F2A6C41-9B3E-4D58-A0C7-5E1B8D4F2A93}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=blip
VersionInfoVersion={#AppVersion}

; `lowest` is what makes {autopf} resolve to %LOCALAPPDATA%\Programs and keeps
; the whole install UAC-free.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UninstallDisplayIcon={app}\{#AppExe}

ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763

OutputDir=..\dist
OutputBaseFilename=blip-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

; Broadcasts WM_SETTINGCHANGE so a newly-opened terminal picks up PATH without
; a logout. Terminals already running still won't see it — nothing can fix that.
ChangesEnvironment=yes

[Languages]
Name: "cn"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\{#CliExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md";                DestDir: "{app}"; Flags: ignoreversion isreadme

[Tasks]
Name: "path";      Description: "把 blip 命令加入 PATH（推荐）"; GroupDescription: "可选："
Name: "autostart"; Description: "开机自动启动"; GroupDescription: "可选："

[Registry]
; {olddata} appends rather than replaces. The Check guards against a reinstall
; stacking duplicate entries, which is the classic way PATH rots.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{olddata};{app}"; Tasks: path; \
    Check: NotOnPath(ExpandConstant('{app}'))

; The daemon's own tray toggle writes this same value, and calls heal() at
; startup to correct a stale path — so the two can't disagree for long.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
    ValueType: string; ValueName: "blip"; ValueData: """{app}\{#AppExe}"""; \
    Flags: uninsdeletevalue; Tasks: autostart

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\卸载 {#AppName}"; Filename: "{uninstallexe}"

[Run]
Filename: "{app}\{#AppExe}"; Description: "立即启动 {#AppName}"; \
    Flags: nowait postinstall skipifsilent

[Code]

{ ---------------------------------------------------------------------------
  Stopping the running daemon.

  This is the only genuinely hard part of installing blip: a running exe holds
  a file lock, so overwriting it fails outright. Inno's Restart Manager support
  is not reliable here — blipd's window is WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
  and deliberately not a normal top-level window, which is exactly what RM
  looks for.

  So: ask nicely first. `blip --quit` goes over the named pipe and lets the
  daemon tear down its tray icon on the way out. Falling straight to taskkill /F
  skips that teardown and leaves a ghost icon in the tray until the user
  happens to sweep the mouse over it.
  --------------------------------------------------------------------------- }
procedure StopDaemon();
var
  Cli: String;
  Rc: Integer;
begin
  Cli := ExpandConstant('{app}\{#CliExe}');
  if FileExists(Cli) then
  begin
    Exec(Cli, '--quit', '', SW_HIDE, ewWaitUntilTerminated, Rc);
    Sleep(600);   { let the message loop unwind and the file lock drop }
  end;

  { Belt and braces: a hung daemon, or one predating --quit, still has to go. }
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '',
       SW_HIDE, ewWaitUntilTerminated, Rc);
  Sleep(300);
end;

{ ---------------------------------------------------------------------------
  PATH handling.

  Read this before "simplifying" either function into a ';'-splitting loop.

  In Inno's Pascal Script, Pos() and Copy()/Length() disagree about what a
  string index means as soon as the string contains non-ASCII characters: on a
  machine whose user name is CJK, Pos(';', S) came back 40 for a semicolon that
  Copy() places at 38 — off by exactly the number of wide characters before it.
  A hand-rolled splitter therefore reads every segment shifted, matches nothing,
  and appends a duplicate PATH entry on every reinstall. Observed, not theorised.

  So: no index arithmetic on these strings. Presence is a delimiter-anchored
  Pos() test that only looks at zero vs non-zero, and removal is StringChangeEx,
  which does its own scanning internally.

  Wrapping both sides in ';' is what keeps this from being a naive substring
  test — without it, "C:\blip" would look present merely because "C:\blip-old"
  is on the PATH.
  --------------------------------------------------------------------------- }
function UserPath(): String;
var
  V: String;
begin
  { Read into a local, not straight into Result: passing the function result
    variable as a var argument is not reliable in Pascal Script. }
  if RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', V) then
    Result := V
  else
    Result := '';
end;

function NotOnPath(Dir: String): Boolean;
var
  Hay: String;
begin
  Hay := ';' + Lowercase(UserPath()) + ';';
  Dir := Lowercase(RemoveBackslashUnlessRoot(Dir));
  { Both spellings, because a directory with and without a trailing backslash
    is the same directory but not the same string. }
  Result := (Pos(';' + Dir + ';', Hay) = 0) and (Pos(';' + Dir + '\;', Hay) = 0);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopDaemon();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  StopDaemon();
  Result := True;
end;

{ Take our segment back out on uninstall. The [Registry] olddata trick can only
  append, so removal has to be explicit.

  StringChangeEx is case-sensitive, which is fine here: we only ever remove the
  exact string the installer wrote, and that is ExpandConstant of the same
  constant both times. A differently-cased entry the user added by hand is
  theirs, and leaving it alone is the safer failure.
  (Braces cannot appear inside a Pascal comment — they close it.) }
procedure RemoveFromPath(Dir: String);
var
  Cur, Hay: String;
begin
  Cur := UserPath();
  Log('RemoveFromPath: dir=[' + Dir + '] pathlen=' + IntToStr(Length(Cur)));
  if Cur = '' then Exit;

  Dir := RemoveBackslashUnlessRoot(Dir);
  Hay := ';' + Cur + ';';
  StringChangeEx(Hay, ';' + Dir + ';', ';', True);
  StringChangeEx(Hay, ';' + Dir + '\;', ';', True);

  { Drop the sentinel delimiters. Length-based indices are safe — it is Pos()
    that disagrees with them, not Delete(). }
  while (Length(Hay) > 0) and (Hay[1] = ';') do
    Delete(Hay, 1, 1);
  while (Length(Hay) > 0) and (Hay[Length(Hay)] = ';') do
    Delete(Hay, Length(Hay), 1);

  if Hay <> Cur then
  begin
    Log('RemoveFromPath: rewriting, newlen=' + IntToStr(Length(Hay)));
    RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Hay);
  end
  else
    Log('RemoveFromPath: no change');
end;

{ ---------------------------------------------------------------------------
  Uninstall cleanup.

  Runs at usUninstall — before the files go — rather than in
  DeinitializeUninstall, where the app constant is no longer dependable
  (braces are omitted here on purpose: they end a Pascal comment). Getting that wrong
  is silent: RemoveFromPath simply never matches, the uninstaller reports
  success, and a dead directory stays on PATH forever.

  %APPDATA%\blip is left alone unless the user says otherwise. A config file is
  their work, not ours, and deleting it on uninstall makes "reinstall to fix a
  problem" a destructive act.
  --------------------------------------------------------------------------- }
procedure CurUninstallStepChanged(CurStep: TUninstallStep);
var
  Cfg: String;
begin
  if CurStep = usUninstall then
    RemoveFromPath(ExpandConstant('{app}'));

  if CurStep = usPostUninstall then
  begin
    Cfg := ExpandConstant('{userappdata}\blip');
    { Never ask in an unattended run, and make "No" the default button when we
      do. /SUPPRESSMSGBOXES answers with the default, so if deletion were the
      default a scripted uninstall would silently destroy the user's config. }
    if DirExists(Cfg) and (not UninstallSilent) then
      if MsgBox('是否一并删除配置文件？' + #13#10 + #13#10 + Cfg,
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(Cfg, True, True, True);
  end;
end;

