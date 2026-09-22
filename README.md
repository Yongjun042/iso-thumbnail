# iso-thumbnail

Blu-ray `.iso` 이미지 안에 들어 있는 디스크 아트워크(`BDMV/META/DL/*.jpg`)를 Windows 탐색기에서
그 `.iso` 파일의 썸네일로 보여 주는 셸 확장(썸네일 핸들러)입니다. 빌드 결과물은 `IsoPreview.dll`과
`isopreview-cli.exe`입니다.

- Rust로 작성한 네이티브 DLL 하나(약 200 KB)로 동작하며, 별도의 런타임이 필요 없습니다.
- 이미지 전체를 읽지 않고 필요한 섹터만 읽기 때문에, 수십 GB짜리 이미지라도 1 ms 안팎에 아트워크를 찾습니다.
- UDF 1.02 ~ 2.60(메타데이터 파티션, 스페어러블 파티션, 가상 파티션/VAT 포함)과 ISO 9660 + Joliet를 읽습니다.
- 시스템 전체 설치(HKLM)에서는 셸이 핸들러를 격리된 COM 대리 프로세스(`dllhost.exe`)에서 실행하므로, 문제가 생겨도 탐색기 자체에는 영향이 없습니다.
  현재 사용자 설치(HKCU)에서는 Windows의 대리 프로세스가 사용자별 COM 등록을 보지 못하기 때문에, 핸들러가 탐색기 프로세스 안에서 직접 실행되도록 등록합니다.
- 신뢰할 수 없는 파일을 파싱하는 코드는 모두 경계 검사를 거치며, 패닉은 COM 경계에서 오류 코드로 바뀝니다.

## 썸네일을 찾는 순서

1. `BDMV/META/DL/*.jpg` : Blu-ray Disc Library 썸네일입니다. 여러 개가 있으면 파일 이름의 `WxH`가 가장 큰 것을, 그것도 없으면 파일 크기가 가장 큰 것을 고릅니다.
2. `BDMV/META/TN/*.jpg` : 트랙 이름 메타데이터의 썸네일입니다.
3. 루트 폴더의 `folder`, `cover`, `poster`, `thumbnail`, `thumb`, `front`, `artwork` + `.jpg/.jpeg/.png/.bmp/.gif` : 일반 데이터 디스크용 대체 경로입니다.

아무것도 없으면 핸들러가 실패를 돌려주고, 탐색기는 평소처럼 기본 `.iso` 아이콘을 보여 줍니다.

## 빌드

Rust 1.82 이상과 Visual Studio Build Tools(MSVC 링커)가 필요합니다.

```bash
cargo build --release
```

결과물은 `target\release\IsoPreview.dll`(셸 확장)과 `target\release\isopreview-cli.exe`(설치 및 진단 도구)입니다.

## 설치

### 현재 사용자만 (관리자 권한 불필요)

```bash
scripts\install.cmd
```

두 파일을 `%LocalAppData%\Programs\IsoPreview`에 복사하고 `HKEY_CURRENT_USER`에 핸들러를 등록합니다.
이 방식에서는 셸의 격리 대리 프로세스가 HKCU 등록을 찾지 못하므로 `DisableProcessIsolation=1`을 함께 기록해서
핸들러가 탐색기 안에서 바로 실행되게 합니다. 탐색기가 DLL을 계속 물고 있으므로, 업데이트나 제거 뒤에는
탐색기를 다시 시작해야 파일을 바꾸거나 지울 수 있습니다.

### 모든 사용자

관리자 권한으로 연 프롬프트에서 실행합니다. `%ProgramFiles%\IsoPreview`에 복사하고 `HKEY_LOCAL_MACHINE`에 등록합니다.

```bash
scripts\install-machine.cmd
```

### regsvr32로 직접 등록

```bash
regsvr32 /n /i:user IsoPreview.dll
```

위 명령은 현재 사용자에게만 등록합니다. `regsvr32 IsoPreview.dll`은 관리자 권한이면 시스템 전체에, 아니면 현재 사용자에게 등록합니다.

### 설치 후 썸네일이 바로 보이지 않을 때

탐색기는 파일 형식별 핸들러 정보와 썸네일을 캐시합니다. 설치 전에 이미 본 `.iso` 파일이 계속 아이콘으로 보이면
`scripts\restart-explorer.cmd`로 탐색기를 다시 시작하고, 그래도 안 되면 `scripts\clear-thumbnail-cache.cmd`로
썸네일 캐시를 지웁니다. 탐색기 옵션의 "아이콘은 항상 표시하고 미리 보기는 표시 안 함"이 꺼져 있어야 합니다.

## 제거

```bash
scripts\uninstall.cmd
```

시스템 전체 설치는 관리자 프롬프트에서 `scripts\uninstall-machine.cmd`를 실행합니다.
`isopreview-cli --uninstall`이나 `regsvr32 /u /n /i:user IsoPreview.dll`로도 등록을 해제할 수 있습니다.
대리 프로세스가 DLL을 아직 물고 있으면 파일 삭제가 잠시 미뤄질 수 있는데, 탐색기를 다시 시작하면 지울 수 있습니다.

## 명령줄 도구로 확인하기

```bash
isopreview-cli movie.iso
```

파서만 실행해서 어느 파일 시스템(UDF 2.50 등)에서 어떤 아트워크를 골랐는지, 읽기 횟수와 소요 시간을 출력하고
원본 JPEG를 `movie.thumb.jpg`로 저장합니다.

```bash
isopreview-cli movie.iso --mode com --size 256
```

`IsoPreview.dll`을 COM 객체로 직접 로드해서 `IThumbnailProvider::GetThumbnail`을 호출하고 결과를 PNG로 저장합니다.
등록하지 않아도 동작하므로 DLL 자체를 시험할 때 씁니다.

```bash
isopreview-cli movie.iso --mode shell --size 256
```

등록된 핸들러를 Windows 셸(`IShellItemImageFactory`)을 통해 호출합니다. 탐색기가 하는 것과 같은 경로이므로
등록 상태까지 함께 확인할 수 있습니다.

`isopreview-cli --install`, `--install-machine`, `--uninstall`은 설치 스크립트가 내부적으로 쓰는 등록 명령입니다.

## 동작 원리

1. 탐색기가 `.iso` 파일의 썸네일 핸들러(CLSID `{C767266A-4032-4099-9A92-F91D1FE98122}`)를 찾아 파일의 `IStream`을 넘겨줍니다.
2. 핸들러는 섹터 256의 Anchor Volume Descriptor Pointer에서 시작해 볼륨 디스크립터, 파티션 맵, File Set Descriptor,
   루트 디렉터리를 차례로 읽고 `BDMV/META/DL`까지 내려갑니다. UDF가 없으면 ISO 9660(Joliet 우선)으로 다시 시도합니다.
3. 고른 JPEG/PNG를 Windows Imaging Component로 디코딩하고, 요청받은 크기(`cx`)를 넘지 않게 축소한 뒤
   32비트 DIB(`HBITMAP`)로 돌려줍니다. 원본보다 키우지는 않습니다.

## 보안과 성능 설계

핸들러는 사용자가 내려받은 임의의 파일을 파싱하고, 사용자별 설치에서는 탐색기 프로세스 안에서 실행됩니다.
그래서 다음과 같은 상한을 두고, 모든 파싱은 경계 검사를 거치며 정수 오버플로도 릴리스 빌드에서 검사합니다.

| 항목 | 상한 | 이유 |
| --- | --- | --- |
| 이미지에서 읽는 총량 | 32 MiB | 조작된 이미지가 긴 읽기를 유발하지 못하게 합니다. |
| 아트워크 파일 크기 | 16 MiB | Blu-ray 썸네일은 수백 KiB이며, 루트 커버 대체 경로에만 영향을 줍니다. |
| 디코딩할 원본 픽셀 수 | 1600만 픽셀 | 거대 이미지가 탐색기 메모리를 잠식하지 못하게 합니다. |
| 결과 썸네일 한 변 | 2560 px | 셸이 요청하는 최대 크기입니다. |
| 디렉터리 크기, 항목 수 | 4 MiB, 16,384개 | 디렉터리 순회 비용을 제한합니다. |
| 파일당 익스텐트, 간접 참조 깊이 | 2,048개, 8단계 | 할당 디스크립터 체인을 따라가는 비용을 제한합니다. |

- 패닉은 `IThumbnailProvider::GetThumbnail` 경계에서 붙잡아 `E_FAIL`로 바꾸므로 탐색기까지 전파되지 않습니다.
- C 런타임을 정적으로 링크해서 `VCRUNTIME140.dll`(VC++ 재배포 패키지)이 없는 PC에서도 로드됩니다.
- JPEG는 WIC 디코더의 `IWICBitmapSourceTransform`을 통해 필요한 크기에 가까운 축소 해상도로 바로 디코딩하고,
  PNG 등 축소 디코딩을 지원하지 않는 형식만 전체 디코딩 후 축소합니다.
- 디렉터리 조회는 필요한 항목을 찾는 즉시 멈추고, 아트워크 후보만 수집합니다.
- 이미지를 한 번 읽을 때마다 32 KiB 단위로 캐시하므로, 실제 Blu-ray 구조를 따라가는 데 필요한 읽기는 10회 안팎입니다.
- `tests/parsers.rs`는 합성 ISO 9660/UDF 이미지(메타데이터 파티션 포함)로 정상 경로와 잘린 이미지, 무작위로 손상된 이미지를 검사합니다.

## 등록되는 레지스트리 키

`HKCU\Software\Classes` 또는 `HKLM\Software\Classes` 아래에 다음 키가 만들어집니다.

- `CLSID\{C767266A-4032-4099-9A92-F91D1FE98122}\InprocServer32` : DLL 경로, `ThreadingModel=Apartment`
- `.iso\ShellEx\{E357FCCD-A995-4576-B01F-234630154E96}` : 위 CLSID

시스템 전체 설치일 때는 `HKLM\...\Shell Extensions\Approved`에도 CLSID를 추가하고, 현재 사용자 설치일 때는
CLSID 키에 `DisableProcessIsolation=1`(DWORD)을 추가합니다.

## 테스트 이미지 만들기

실제 Blu-ray 이미지가 없어도 Windows에 내장된 IMAPI2로 시험용 이미지를 만들 수 있습니다.

```bash
powershell -ExecutionPolicy Bypass -File scripts\make-test-isos.ps1 -OutDir C:\temp\test-isos
```

UDF 2.50(메타데이터 파티션), UDF 2.01, UDF 1.02, ISO 9660 + Joliet + UDF 브리지, Joliet 전용, ISO 9660 전용,
TN 폴더만 있는 이미지, 아트워크가 없는 이미지, 루트 커버만 있는 이미지 등 10종이 생성됩니다.

## 제한 사항

- 스페어러블 파티션의 스페어링 테이블은 무시합니다. 결함 섹터가 재배치된 디스크 이미지는 드물기 때문입니다.
- DVD-Video의 `JACKET_P` 정지 화상(MPEG)은 지원하지 않습니다.
- `.iso` 확장자에만 연결됩니다. 다른 확장자에도 붙이려면 `src/registry.rs`의 `EXTENSIONS` 목록에 추가하고 다시 빌드하면 됩니다.

## 라이선스

MIT
