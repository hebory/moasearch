#ifndef ProductVersion
  #error ProductVersion must be supplied by packaging/release.ps1
#endif
#ifndef ProductFileVersion
  #error ProductFileVersion must be supplied by packaging/release.ps1
#endif
#ifndef PayloadDir
  #error PayloadDir must be supplied by packaging/release.ps1
#endif
#ifndef BrandingIcon
  #error BrandingIcon must be supplied by packaging/release.ps1
#endif

[Setup]
AppId={{9128CC61-7F3D-416D-AEF5-24B29670C8FE}
AppName=모아검색
AppVersion={#ProductVersion}
AppVerName=모아검색 {#ProductVersion}
AppPublisher=hebory
AppPublisherURL=https://github.com/hebory/moasearch
AppSupportURL=https://github.com/hebory/moasearch/issues
AppUpdatesURL=https://github.com/hebory/moasearch/releases
DefaultDirName={localappdata}\Programs\moasearch
DefaultGroupName=모아검색
DisableProgramGroupPage=yes
UsePreviousAppDir=no
PrivilegesRequired=lowest
MinVersion=10.0
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64
OutputDir={#OutputDir}
OutputBaseFilename=moasearch-{#ProductVersion}-win-x64-setup
SetupIconFile={#BrandingIcon}
UninstallDisplayName=모아검색 {#ProductVersion}
UninstallDisplayIcon={app}\moasearch.exe
CloseApplications=yes
CloseApplicationsFilter=moasearch.exe
RestartApplications=no
WizardStyle=modern
Compression=lzma2/normal
SolidCompression=yes
VersionInfoVersion={#ProductFileVersion}
VersionInfoProductVersion={#ProductFileVersion}
VersionInfoProductTextVersion={#ProductVersion}
VersionInfoCompany=hebory
VersionInfoProductName=MoaSearch
VersionInfoDescription=MoaSearch per-user installer
VersionInfoOriginalFileName=moasearch-{#ProductVersion}-win-x64-setup.exe

[Languages]
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"

[Tasks]
Name: "desktopicon"; Description: "바탕 화면 바로 가기 만들기"; GroupDescription: "바로 가기:"; Flags: unchecked
Name: "explorercontext"; Description: "탐색기에서 현재 폴더 검색"; GroupDescription: "탐색기:"

[Registry]
; Use app-specific static verbs for the selected folder and its background.
; Append \. so a drive root cannot escape the closing command-line quote.
Root: HKCU; Subkey: "Software\Classes\Directory\shell\MoaSearch.SearchFolder"; ValueType: string; ValueName: ""; ValueData: "모아검색으로 여기서 검색"; Tasks: explorercontext; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\MoaSearch.SearchFolder"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\moasearch.exe,0"; Tasks: explorercontext
Root: HKCU; Subkey: "Software\Classes\Directory\shell\MoaSearch.SearchFolder"; ValueType: string; ValueName: "MultiSelectModel"; ValueData: "Single"; Tasks: explorercontext
Root: HKCU; Subkey: "Software\Classes\Directory\shell\MoaSearch.SearchFolder\command"; ValueType: string; ValueName: ""; ValueData: """{app}\moasearch.exe"" open-folder --path ""%1\."""; Tasks: explorercontext; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\MoaSearch.SearchFolder"; ValueType: none; Tasks: not explorercontext; Flags: deletekey
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\MoaSearch.SearchBackground"; ValueType: string; ValueName: ""; ValueData: "모아검색으로 여기서 검색"; Tasks: explorercontext; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\MoaSearch.SearchBackground"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\moasearch.exe,0"; Tasks: explorercontext
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\MoaSearch.SearchBackground"; ValueType: string; ValueName: "MultiSelectModel"; ValueData: "Single"; Tasks: explorercontext
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\MoaSearch.SearchBackground\command"; ValueType: string; ValueName: ""; ValueData: """{app}\moasearch.exe"" open-folder --path ""%V\."""; Tasks: explorercontext; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\MoaSearch.SearchBackground"; ValueType: none; Tasks: not explorercontext; Flags: deletekey

[Files]
; Extract to the installer temporary directory in PrepareToInstall, before app files are installed.
Source: "{#PayloadDir}\MicrosoftEdgeWebview2Setup.exe"; Flags: dontcopy
Source: "{#PayloadDir}\moasearch.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\onnxruntime.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\onnxruntime_providers_shared.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\MSVCP140.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\VCRUNTIME140.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\VCRUNTIME140_1.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\ONNX-Runtime-LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\ONNX-Runtime-ThirdPartyNotices.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\모아검색"; Filename: "{app}\moasearch.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\모아검색"; Filename: "{app}\moasearch.exe"; WorkingDir: "{app}"; Tasks: desktopicon
Name: "{autoprograms}\모아검색 제거"; Filename: "{uninstallexe}"

[Run]
Filename: "{app}\moasearch.exe"; Description: "모아검색 실행"; WorkingDir: "{app}"; Flags: postinstall nowait skipifsilent

[Code]
const
  WebView2RuntimeClientId = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';
  WebView2RuntimeInstaller = 'MicrosoftEdgeWebview2Setup.exe';
  WebView2RuntimeDownloadPage = 'https://developer.microsoft.com/microsoft-edge/webview2';

function IsInstalledWebView2Version(const VersionString: string): Boolean;
var
  I, StartPosition, ComponentValue, ComponentCount: Integer;
  HasNonzeroComponent: Boolean;
  Part: string;
begin
  Result := False;
  if VersionString = '' then
    Exit;

  StartPosition := 1;
  ComponentCount := 0;
  HasNonzeroComponent := False;
  for I := 1 to Length(VersionString) do
  begin
    if VersionString[I] = '.' then
    begin
      Part := Copy(VersionString, StartPosition, I - StartPosition);
      if Part = '' then
        Exit;
      ComponentValue := StrToIntDef(Part, -1);
      if ComponentValue < 0 then
        Exit;
      if ComponentValue > 0 then
        HasNonzeroComponent := True;
      Inc(ComponentCount);
      StartPosition := I + 1;
    end
    else if (VersionString[I] < '0') or (VersionString[I] > '9') then
      Exit;
  end;

  Part := Copy(VersionString, StartPosition, Length(VersionString) - StartPosition + 1);
  if Part = '' then
    Exit;
  ComponentValue := StrToIntDef(Part, -1);
  if ComponentValue < 0 then
    Exit;
  if ComponentValue > 0 then
    HasNonzeroComponent := True;
  Inc(ComponentCount);

  Result := HasNonzeroComponent and (ComponentCount >= 2);
end;

function IsWebView2RuntimeInstalled: Boolean;
var
  VersionString: string;
begin
  Result := False;
  if RegQueryStringValue(HKLM32,
    'SOFTWARE\Microsoft\EdgeUpdate\Clients\' + WebView2RuntimeClientId,
    'pv', VersionString) and IsInstalledWebView2Version(VersionString) then
  begin
    Result := True;
    Exit;
  end;

  if RegQueryStringValue(HKCU32,
    'Software\Microsoft\EdgeUpdate\Clients\' + WebView2RuntimeClientId,
    'pv', VersionString) and IsInstalledWebView2Version(VersionString) then
  begin
    Result := True;
    Exit;
  end;

  if RegQueryStringValue(HKCU,
    'Software\Microsoft\EdgeUpdate\Clients\' + WebView2RuntimeClientId,
    'pv', VersionString) and IsInstalledWebView2Version(VersionString) then
    Result := True;
end;

function PrepareToInstall(var NeedsRestart: Boolean): string;
var
  Started: Boolean;
  ResultCode: Integer;
  ErrorText: string;
begin
  Result := '';
  NeedsRestart := False;
  if IsWebView2RuntimeInstalled then
    Exit;

  ExtractTemporaryFile(WebView2RuntimeInstaller);
  Started := Exec(ExpandConstant('{tmp}\' + WebView2RuntimeInstaller),
    '/silent /install', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);

  { Some runtime versions return a non-zero status after a successful install;
    the documented registry marker is the final source of truth. }
  if IsWebView2RuntimeInstalled then
    Exit;

  ErrorText := 'Microsoft Edge WebView2 Runtime을 설치하지 못했습니다.' + #13#10 +
    '모아검색 데스크톱 창을 표시하려면 WebView2 Runtime이 필요합니다.' + #13#10 +
    '인터넷 연결을 확인한 뒤 설치 프로그램을 다시 실행하거나 다음 페이지에서 Runtime을 설치하세요:' + #13#10 +
    WebView2RuntimeDownloadPage;
  if not Started then
    ErrorText := ErrorText + #13#10 + '설치 프로그램을 시작하지 못했습니다.'
  else
    ErrorText := ErrorText + #13#10 + '설치 종료 코드: ' + IntToStr(ResultCode);
  Result := ErrorText;
end;
