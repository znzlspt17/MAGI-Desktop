# Windows 개발 안내

## 준비

Windows에서 Rust toolchain과 Cargo를 준비합니다. GUI를 포함한 빌드에는 Windows MSVC 빌드 도구가 필요합니다. 형식 검사와 Clippy를 사용하려면 해당 Rust 구성 요소도 설치되어 있어야 합니다. 아래 명령은 저장소 루트에서 PowerShell로 실행합니다.

## 검사

GUI를 제외한 workspace의 형식 검사, 테스트 및 Clippy를 실행합니다.

```powershell
pwsh -NoProfile -File ./scripts/verify-headless.ps1
```

전체 workspace를 검사하려면 다음 명령을 사용할 수 있습니다.

```powershell
cargo fmt --all -- --check
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

## Windows 개발 빌드

```powershell
pwsh -NoProfile -File ./scripts/build-windows.ps1
```

이 명령은 workspace의 release 빌드를 실행하고 매번 새로운 `target/windows-build-<식별자>` 폴더를 만듭니다. 성공 시 출력된 폴더의 `release` 아래에 `desktop.exe`, `engine.exe`, `inference.exe`가 생성됩니다. 세 프로그램은 별도 프로세스입니다. 빌드 스크립트 자체는 배포 패키지나 제3자 라이선스 고지를 생성하지 않습니다.

## 개발용 ZIP

빌드 출력에 표시된 폴더의 `release` 경로를 지정합니다. 아래 경로는 실제 출력된 경로로 바꿔야 합니다.

```powershell
pwsh -NoProfile -File ./scripts/package-windows.ps1 -BuildDirectory './target/windows-build-<식별자>/release'
```

지정한 폴더의 세 실행 파일과 안내문, `THIRD-PARTY-NOTICES.txt`를 새 `dist/windows-<식별자>` 폴더 및 ZIP에 담습니다. 라이선스 고지 수집은 현재 workspace의 잠금 의존성과 로컬에 있는 공개 Cargo 소스를 사용하며 네트워크 없이 실행됩니다. 필요한 의존성이 로컬에 없으면 패키징이 실패할 수 있습니다.

고지 파일에는 찾지 못한 원문이 표시됩니다. 입력 실행 파일이 현재 workspace에서 만들어졌는지는 확인하지 않으므로, 배포 전에는 빌드 출처와 누락된 고지를 별도로 확인해야 합니다. 이 ZIP은 개발용이며 배포 준비가 완료된 패키지가 아닙니다. 실제 모델 추론은 아직 지원하지 않습니다.
