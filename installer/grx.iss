; Installateur GamingRemoteExperience (Inno Setup 6).
; Construit par tools\build-setup.ps1 (appelé par tools\release.ps1) :
;   ISCC.exe /DAppVersion=0.2.0 /DBinDir=<dossier des exe> /DVigemExe=<ViGEmBus vérifié> /DOutDir=<sortie> installer\grx.iss
;
; - Le dialogue « pour moi seulement / pour tous les utilisateurs » est proposé au lancement
;   (PrivilegesRequired=lowest + PrivilegesRequiredOverridesAllowed) ; /CURRENTUSER ou /ALLUSERS
;   le choisissent en ligne de commande (mises à jour silencieuses).
; - Les données (%APPDATA%\GamingRemoteExperience : identités, clients autorisés, hosts) sont
;   CONSERVÉES à la désinstallation, sauf si l'utilisateur demande leur suppression.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif
#ifndef OutDir
  #define OutDir "..\dist"
#endif

#define AppName "GamingRemoteExperience"
#define DataDirName "GamingRemoteExperience"
#define FirewallRule "GamingRemoteExperience (grx-host)"

[Setup]
; Identifiant FIXE : ne jamais le changer (sinon les mises à jour s'installeraient à côté).
AppId={{6B0E3A52-5F0C-4C7E-9B7B-3E2B8D7E4A11}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppName}
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} - installateur
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
UsePreviousPrivileges=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutDir}
OutputBaseFilename=GRX-Setup-{#AppVersion}
SetupIconFile=grx.ico
UninstallDisplayIcon={app}\grx-gui.exe
UninstallDisplayName={#AppName} {#AppVersion}
WizardStyle=modern
Compression=lzma2/ultra64
SolidCompression=yes
; Ferme les programmes qui utilisent les fichiers à remplacer (Restart Manager) ; les grx-host /
; grx-client sont d'abord arrêtés proprement par grx-gui --stop-grx-processes (voir [Code]).
CloseApplications=force
CloseApplicationsFilter=*.exe
RestartApplications=no
ShowLanguageDialog=auto
ChangesAssociations=yes

[Languages]
Name: "french"; MessagesFile: "compiler:Languages\French.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
french.CompVigem=Pilote manettes ViGEmBus (nécessaire au host pour les manettes ; droits administrateur)
english.CompVigem=ViGEmBus gamepad driver (required by the host for gamepads; administrator rights)
french.CompMain=Programmes GamingRemoteExperience
english.CompMain=GamingRemoteExperience programs
french.TaskAssoc=Ouvrir les liens d'invitation grx:// avec GamingRemoteExperience
english.TaskAssoc=Open grx:// invitation links with GamingRemoteExperience
french.TaskFirewall=Autoriser grx-host dans le pare-feu Windows (UDP entrant)
english.TaskFirewall=Allow grx-host through Windows Firewall (inbound UDP)
french.GroupOther=Autres options :
english.GroupOther=Other options:
french.RunGui=Lancer GamingRemoteExperience
english.RunGui=Launch GamingRemoteExperience
french.InstallingVigem=Installation du pilote manettes ViGEmBus...
english.InstallingVigem=Installing the ViGEmBus gamepad driver...
french.FirewallUserMode=Pare-feu : installation sans droits administrateur, aucune règle n'a été créée. Au premier démarrage du host, Windows demandera l'autorisation (cochez « Privé »), ou lancez « grx-host firewall-help » pour la commande à exécuter en administrateur.
english.FirewallUserMode=Firewall: installed without administrator rights, no rule was created. When the host first starts, Windows will ask for permission (tick "Private"), or run "grx-host firewall-help" for the command to run as administrator.
french.DeleteData=Supprimer aussi vos données GamingRemoteExperience ?%n%n%1%n%nIdentités (clés) de ce PC, clients autorisés et hosts mémorisés. Si vous les supprimez, il faudra refaire toutes les invitations.%n%nChoisissez « Non » pour les conserver (recommandé en cas de réinstallation).
english.DeleteData=Also delete your GamingRemoteExperience data?%n%n%1%n%nThis PC's identities (keys), authorized clients and saved hosts. If you delete them, all invitations will have to be redone.%n%nChoose "No" to keep them (recommended if you plan to reinstall).
french.UrlDesc=URL:Lien d'invitation GamingRemoteExperience
english.UrlDesc=URL:GamingRemoteExperience invitation link

[Types]
Name: "full"; Description: "{code:TypeFull}"
Name: "custom"; Description: "{code:TypeCustom}"; Flags: iscustom

[Components]
Name: "main"; Description: "{cm:CompMain}"; Types: full custom; Flags: fixed
Name: "vigem"; Description: "{cm:CompVigem}"; Types: full; Check: not ViGEmInstalled

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "assoc"; Description: "{cm:TaskAssoc}"; GroupDescription: "{cm:GroupOther}"
Name: "firewall"; Description: "{cm:TaskFirewall}"; GroupDescription: "{cm:GroupOther}"; Check: IsAdminInstallMode

[Files]
Source: "{#BinDir}\grx-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\grx-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\grx-client.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\TUTO.md"; DestDir: "{app}"; Flags: ignoreversion
; Décodeur de secours FFmpeg (LGPL, facultatif : voir tools\build-setup.ps1 -FfmpegDir).
#ifdef FfmpegDir
Source: "{#FfmpegDir}\avcodec-62.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#FfmpegDir}\avutil-60.dll"; DestDir: "{app}"; Flags: ignoreversion
#ifndef FfmpegNoSwr
Source: "{#FfmpegDir}\swresample-6.dll"; DestDir: "{app}"; Flags: ignoreversion
#endif
Source: "{#FfmpegDir}\LICENSE.txt"; DestDir: "{app}\licenses"; DestName: "FFmpeg-LGPL.txt"; Flags: ignoreversion
Source: "{#FfmpegDir}\SOURCE.txt"; DestDir: "{app}\licenses"; DestName: "FFmpeg-SOURCE.txt"; Flags: ignoreversion
#endif
#ifdef VigemExe
Source: "{#VigemExe}"; DestDir: "{tmp}"; DestName: "ViGEmBus_setup.exe"; Components: vigem; Flags: deleteafterinstall
#endif

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\grx-gui.exe"; WorkingDir: "{app}"; Comment: "{#AppName}"
Name: "{autoprograms}\{#AppName} - Tutoriel"; Filename: "{app}\TUTO.md"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\grx-gui.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Registry]
; Protocole grx:// (HKCU en installation utilisateur, HKLM en installation machine). Le lien est
; passé après --open-link : grx-gui ne l'interprète que comme lien (jamais comme option).
Root: HKA; Subkey: "Software\Classes\grx"; ValueType: string; ValueName: ""; ValueData: "{cm:UrlDesc}"; Flags: uninsdeletekey; Tasks: assoc
Root: HKA; Subkey: "Software\Classes\grx"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""; Tasks: assoc
Root: HKA; Subkey: "Software\Classes\grx\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\grx-gui.exe"",0"; Tasks: assoc
Root: HKA; Subkey: "Software\Classes\grx\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\grx-gui.exe"" --open-link ""%1"""; Tasks: assoc

[Run]
; Pilote ViGEmBus : seulement si coché ET absent ; c'est un pilote, il exige l'élévation.
#ifdef VigemExe
Filename: "{tmp}\ViGEmBus_setup.exe"; Parameters: "/exenoui /qn /norestart"; StatusMsg: "{cm:InstallingVigem}"; Components: vigem; Check: (not ViGEmInstalled) and IsAdminInstallMode; Flags: waituntilterminated
Filename: "{tmp}\ViGEmBus_setup.exe"; Parameters: "/exenoui /qn /norestart"; StatusMsg: "{cm:InstallingVigem}"; Components: vigem; Check: (not ViGEmInstalled) and (not IsAdminInstallMode); Flags: shellexec waituntilterminated; Verb: "runas"
#endif
; Pare-feu (installation machine uniquement) : on retire d'abord une éventuelle ancienne règle.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#FirewallRule}"""; Tasks: firewall; Flags: runhidden waituntilterminated
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""{#FirewallRule}"" dir=in action=allow protocol=UDP program=""{app}\grx-host.exe"" enable=yes profile=any"; Tasks: firewall; Flags: runhidden waituntilterminated
Filename: "{app}\grx-gui.exe"; Description: "{cm:RunGui}"; Flags: nowait postinstall skipifsilent
; Mise à jour silencieuse lancée par grx-gui avec /RELAUNCH : on rouvre la GUI (jamais élevée).
Filename: "{app}\grx-gui.exe"; Flags: nowait runasoriginaluser; Check: RelaunchRequested

[UninstallRun]
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#FirewallRule}"""; Tasks: firewall; Flags: runhidden waituntilterminated; RunOnceId: "DelFirewallRule"
; Contrôle total : supprime la tâche planifiée « host élevé » (sinon elle pointerait vers un exe supprimé).
; Sans effet si la tâche n'existe pas. Uniquement en installation machine (seule à pouvoir l'activer).
Filename: "{app}\grx-host.exe"; Parameters: "elevation remove-task"; Check: IsAdminInstallMode; Flags: runhidden waituntilterminated; RunOnceId: "DelElevatedTask"

[Code]
function TypeFull(Param: String): String;
begin
  if ActiveLanguage = 'french' then Result := 'Installation complète' else Result := 'Full installation';
end;

function TypeCustom(Param: String): String;
begin
  if ActiveLanguage = 'french' then Result := 'Installation personnalisée' else Result := 'Custom installation';
end;

function ViGEmInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\ViGEmBus');
end;

function HasParam(Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
      Result := True;
end;

function RelaunchRequested: Boolean;
begin
  Result := WizardSilent and HasParam('/RELAUNCH');
end;

{ Arrêt propre des grx-host / grx-client de ce dossier (Ctrl+C : le host referme la redirection
  de la box), puis fermeture des autres fenêtres grx-gui. Les processus d'autres dossiers ne
  sont jamais touchés. }
procedure StopGrxProcesses(Dir: String);
var
  Code: Integer;
begin
  if FileExists(Dir + '\grx-gui.exe') then
    Exec(Dir + '\grx-gui.exe', '--stop-grx-processes', Dir, SW_HIDE, ewWaitUntilTerminated, Code);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopGrxProcesses(ExpandConstant('{app}'));
  Result := '';
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpFinished) and (not IsAdminInstallMode) then
    WizardForm.FinishedLabel.Caption := WizardForm.FinishedLabel.Caption + #13#10#13#10 + CustomMessage('FirewallUserMode');
end;

function InitializeUninstall: Boolean;
begin
  StopGrxProcesses(ExpandConstant('{app}'));
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
  Delete: Boolean;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    DataDir := ExpandConstant('{userappdata}\{#DataDirName}');
    if not DirExists(DataDir) then
      Exit;
    { Par défaut les données sont CONSERVÉES. En silencieux : seulement avec /DELETEDATA. }
    if UninstallSilent then
      Delete := HasParam('/DELETEDATA')
    else
      Delete := MsgBox(FmtMessage(CustomMessage('DeleteData'), [DataDir]), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
    if Delete then
      DelTree(DataDir, True, True, True);
  end;
end;
