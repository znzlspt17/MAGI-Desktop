# MAGI-Desktop

MAGI의 명세 및 에이전트 워크플로를 Rust 기반 Windows 애플리케이션으로 구현하는 프로젝트입니다. GUI, 실행 엔진, 추론 작업자를 별도 프로세스로 구성합니다.

Rust workspace에는 `core`, `desktop`, `engine`, `inference`, `integrations`, `provider`, `transport` 크레이트가 있습니다. Windows 개발 환경에서 빌드, 검사 및 개발용 ZIP을 만드는 방법은 [Windows 개발 안내](docs/BUILD-WINDOWS.md)를 참고하세요.

현재 실제 모델 추론은 지원하지 않습니다. 개발용 ZIP은 배포 준비가 완료된 패키지가 아니며, 포함된 제3자 고지와 누락 항목을 배포 전에 검토해야 합니다.
