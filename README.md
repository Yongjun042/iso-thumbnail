# iso-thumbnail

Blu-ray `.iso` 이미지 안에 들어 있는 디스크 아트워크(`BDMV/META/DL/*.jpg`)를 Windows 탐색기에서
그 `.iso` 파일의 썸네일로 보여 주는 셸 확장(썸네일 핸들러)입니다. 빌드 결과물은 `IsoPreview.dll`과
`isopreview-cli.exe`입니다.

- Rust로 작성한 네이티브 DLL 하나(약 320 KB)로 동작하며, 별도의 런타임이 필요 없습니다.
- 이미지 전체를 읽지 않고 필요한 섹터만 읽기 때문에, 수십 GB짜리 이미지라도 1 ms 안팎에 아트워크를 찾습니다.
- UDF 1.02 ~ 2.60(메타데이터 파티션, 스페어러블 파티션, 가상 파티션/VAT 포함)과 ISO 9660 + Joliet를 읽습니다.
- 시스템 전체 설치(HKLM)와 현재 사용자 설치(HKCU) 모두 셸이 핸들러를 격리된 COM 대리 프로세스(`dllhost.exe`)에서 실행하므로,
  문제가 생겨도 탐색기 자체에는 영향이 없습니다.
- 신뢰할 수 없는 파일을 파싱하는 코드는 모두 경계 검사를 거치며, 패닉은 COM 경계에서 오류 코드로 바뀝니다.

## 썸네일을 찾는 순서

1. `BDMV/META/DL/*.jpg` : Blu-ray Disc Library 썸네일입니다. 여러 개가 있으면 파일 이름의 `WxH`가 가장 큰 것을, 그것도 없으면 파일 크기가 가장 큰 것을 고릅니다.
2. `BDMV/META/TN/*.jpg` : 트랙 이름 메타데이터의 썸네일입니다.
3. 루트 폴더의 `folder`, `cover`, `poster`, `thumbnail`, `thumb`, `front`, `artwork` + `.jpg/.jpeg/.png/.bmp/.gif` : 일반 데이터 디스크용 대체 경로입니다.

어느 단계에서든 비어 있거나 16 MiB를 넘거나 읽을 수 없는 파일은 건너뛰고 다음 후보를 봅니다.
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
시스템 전체 설치와 마찬가지로 핸들러는 탐색기가 아니라 셸의 격리된 대리 프로세스(`dllhost.exe`)에서 실행되며,
이 대리 프로세스는 HKCU 등록도 찾습니다. `isopreview-cli.exe`로 등록하지 못하면(백신이 서명 없는 실행 파일을
막는 경우 등) `regsvr32 /n /i:user`로 다시 등록합니다.

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

설치와 제거는 일반 명령 프롬프트나 Windows 터미널에서 실행하세요. Claude 데스크톱 앱처럼 MSIX로 패키징된 앱 안의
터미널에서 실행하면 `HKCU\Software\Classes`에 쓴 등록이 그 앱 전용 가상 레지스트리에 기록되어 탐색기가 보지 못합니다.
같은 앱 안에서 `reg query`로 확인하면 등록이 보이기 때문에 설치가 된 것처럼 보이니 주의하세요.

## 제거

```bash
scripts\uninstall.cmd
```

시스템 전체 설치는 관리자 프롬프트에서 `scripts\uninstall-machine.cmd`를 실행합니다.
`isopreview-cli --uninstall`이나 `regsvr32 /u /n /i:user IsoPreview.dll`로도 등록을 해제할 수 있으며,
`uninstall.cmd`는 CLI를 실행하지 못하면 `regsvr32`로 해제합니다.
썸네일 대리 프로세스(`dllhost.exe`)가 DLL을 아직 물고 있으면 파일 삭제가 잠시 미뤄질 수 있습니다. 대리 프로세스는 한동안
쓰이지 않으면 스스로 끝나므로 조금 뒤에 다시 지우면 됩니다. 이전 버전으로 설치해서 탐색기가 DLL을 직접 물고 있다면
탐색기를 다시 시작하세요.

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
3. 고른 파일을 파일 서명에 맞는 Windows 기본 디코더(JPEG, PNG, GIF, BMP)로 디코딩하고, 요청받은 크기(`cx`)를
   넘지 않게 축소한 뒤 32비트 DIB(`HBITMAP`)로 돌려줍니다. 원본보다 키우지는 않습니다.

## 보안과 성능 설계

핸들러는 사용자가 내려받은 임의의 파일을 파싱합니다. 탐색기에서는 격리된 대리 프로세스에서 실행되지만, 셸 API로
핸들러를 자기 프로세스 안에 직접 불러오는 프로그램도 있습니다. 그래서 다음과 같은 상한을 두고, 모든 파싱은 경계 검사를
거치며 정수 오버플로도 릴리스 빌드에서 검사합니다.

| 항목 | 상한 | 이유 |
| --- | --- | --- |
| 이미지에서 읽는 총량 | 32 MiB | 조작된 이미지가 긴 읽기를 유발하지 못하게 합니다. |
| 이미지에 대한 읽기 횟수 | 256회 | 하드 디스크나 네트워크 공유에서는 탐색(seek) 횟수가 시간을 좌우합니다. |
| 읽기 요청 하나당 `IStream::Read` 호출 | 64회 | 데이터를 조금씩 돌려주는 스트림에서도 호출 수가 늘어나지 않게 합니다. |
| 디렉터리 하나에서 비교하는 그림 파일 | 64개 | 후보마다 드는 파일 엔트리 조회 비용을 제한합니다. |
| 아트워크 파일 크기 | 16 MiB | Blu-ray 썸네일은 수백 KiB이며, 루트 커버 대체 경로에만 영향을 줍니다. |
| 디코딩할 원본 픽셀 수 | 1600만 픽셀 | 거대 이미지가 핸들러를 실행하는 프로세스의 메모리를 잠식하지 못하게 합니다. |
| 결과 썸네일 한 변 | 2560 px | 셸이 요청하는 최대 크기입니다. |
| 디렉터리 크기, 항목 수 | 4 MiB, 16,384개 | 디렉터리 순회 비용을 제한합니다. |
| 파일당 익스텐트, 간접 참조 깊이 | 2,048개, 8단계 | 할당 디스크립터 체인을 따라가는 비용을 제한합니다. |

- 패닉은 `IThumbnailProvider::GetThumbnail` 경계에서 붙잡아 `E_FAIL`로 바꾸므로 핸들러를 부른 프로세스로 전파되지 않습니다.
- 디코더는 파일 서명으로 고른 Windows 기본 JPEG/PNG/GIF/BMP 디코더만 씁니다. WIC가 내용을 보고 코덱을 고르게 두면
  확장자만 `.jpg`인 TIFF, JPEG XR, RAW 같은 데이터가 설치된 아무 코덱에나 넘어가고, 일부 코덱은 위의 픽셀 상한과
  무관하게 수백 MB를 할당합니다.
- 핸들러 객체는 `ThreadingModel=Apartment`에 맞게 agile로 동작하지 않으므로, 다른 아파트에서의 호출은 COM이 직렬화합니다.
  한 번 초기화된 객체의 재초기화는 `ERROR_ALREADY_INITIALIZED`로 거부합니다.
- `DllCanUnloadNow`는 살아 있는 핸들러와 클래스 팩토리, `LockServer` 잠금이 모두 없어진 뒤에만 언로드를 허락합니다.
- C 런타임을 정적으로 링크해서 `VCRUNTIME140.dll`(VC++ 재배포 패키지)이 없는 PC에서도 로드됩니다.
- JPEG는 WIC 디코더의 `IWICBitmapSourceTransform`을 통해 필요한 크기에 가까운 축소 해상도로 바로 디코딩하고,
  PNG 등 축소 디코딩을 지원하지 않는 형식만 전체 디코딩 후 축소합니다.
- 디렉터리 항목 순회는 필요한 항목을 찾는 즉시 멈추고, 루트 디렉터리는 한 번만 읽어서 `BDMV`와 루트 커버 후보를 함께 찾습니다.
- 후보를 비교할 때는 파일 엔트리의 크기 필드만 읽고, 할당 디스크립터는 실제로 읽을 파일에 대해서만 해석합니다.
  메타데이터 파티션의 블록 위치는 이진 탐색으로 찾습니다.
- 이미지를 한 번 읽을 때마다 32 KiB 단위로 캐시하므로, 실제 Blu-ray 구조를 따라가는 데 필요한 읽기는 10회 안팎입니다.
- `tests/parsers.rs`는 합성 ISO 9660/UDF 이미지(메타데이터 파티션 포함)로 정상 경로, Indirect Entry, 깨진 루트 커버를 건너뛰는
  경우와 잘린 이미지, 무작위로 손상된 이미지를 검사합니다.

## 등록되는 레지스트리 키

`HKCU\Software\Classes` 또는 `HKLM\Software\Classes` 아래에 다음 키가 만들어집니다.

- `CLSID\{C767266A-4032-4099-9A92-F91D1FE98122}\InprocServer32` : DLL 경로, `ThreadingModel=Apartment`
- `.iso\ShellEx\{E357FCCD-A995-4576-B01F-234630154E96}` : 위 CLSID

시스템 전체 설치일 때는 `HKLM\...\Shell Extensions\Approved`에도 CLSID를 추가합니다. 어느 범위에서도
`DisableProcessIsolation`은 기록하지 않으므로 핸들러는 격리된 대리 프로세스에서 실행됩니다. 이전 버전이 현재 사용자
설치에 남긴 `DisableProcessIsolation=1`은 다시 설치할 때 지웁니다.

같은 범위에 다른 프로그램의 `.iso` 썸네일 핸들러가 이미 등록되어 있었다면 그 CLSID를 위 CLSID 키의
`PreviousThumbnailHandler.iso` 값에 저장해 두었다가, 등록을 해제할 때 원래대로 되돌립니다.

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
