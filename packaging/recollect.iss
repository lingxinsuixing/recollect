; recollect Windows 安装器（Inno Setup 6）
;
; 设计约束（2026-11-09 用户裁定）：
; - **不自动改任何宿主的 MCP 配置**：安装器只落二进制、可选加 PATH、提供说明书；
; - 每用户安装（PrivilegesRequired=lowest），不需要管理员权限；
; - 升级/卸载**不碰数据目录**（数据目录由 --data-dir 指定，不在 {app} 下）；
; - 六件二进制与便携包同源（dist\windows-x86_64）。
;
; 编译：ISCC.exe /DMyVersion=0.1.2 /DMyPublisher="recollect contributors" packaging\recollect.iss

#ifndef MyVersion
  #define MyVersion "0.1.2"
#endif
#ifndef MyPublisher
  #define MyPublisher "recollect contributors"
#endif

[Setup]
AppId={{8F3C2B74-6E1D-4A57-9C0B-2D7E5A1F4C93}
AppName=recollect
AppVersion={#MyVersion}
AppPublisher={#MyPublisher}
VersionInfoVersion={#MyVersion}
VersionInfoDescription=recollect agent memory service
VersionInfoProductName=recollect
VersionInfoCompany={#MyPublisher}
DefaultDirName={userpf}\recollect
DefaultGroupName=recollect
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=recollect-{#MyVersion}-setup-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=recollect
ChangesEnvironment=yes

[Files]
Source: "..\dist\windows-x86_64\*.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "..\docs\2026-11-09-agent-install-and-import.md"; DestDir: "{app}"; DestName: "README-agent.md"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Tasks]
Name: "addtopath"; Description: "把 recollect 加入 PATH（仅当前用户）"; GroupDescription: "可选："; Flags: checkedonce
Name: "openreadme"; Description: "完成后打开 agent 接入说明书"; GroupDescription: "可选："; Flags: checkedonce

[Run]
Filename: "{app}\README-agent.md"; Description: "打开 agent 接入说明书"; Flags: postinstall shellexec skipifsilent unchecked; Tasks: openreadme

[Code]
const EnvironmentKey = 'Environment';

procedure EnvAddPath(Path: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Paths := '';
  if Pos(';' + Uppercase(Path) + ';', ';' + Uppercase(Paths) + ';') > 0 then
    exit;
  if Paths <> '' then
    Paths := Paths + ';';
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths + Path);
end;

procedure EnvRemovePath(Path: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    exit;
  P := Pos(';' + Uppercase(Path) + ';', ';' + Uppercase(Paths) + ';');
  if P = 0 then
    exit;
  Delete(Paths, P, Length(Path) + 1);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    EnvAddPath(ExpandConstant('{app}\bin'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    EnvRemovePath(ExpandConstant('{app}\bin'));
end;
