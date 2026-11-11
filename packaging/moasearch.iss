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

[Files]
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
