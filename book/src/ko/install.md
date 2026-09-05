# 설치

> 🇬🇧 [English](../install.md)

MemGarden은 저장소 하나에 프로세스 둘 분량의 코드다: 오래 사는 데몬
(`memgardend`)과 Claude Code가 띄우는 훅 바이너리(`memgarden`). 둘 다 root도,
DB 서버도, 네트워크도 필요 없다.

---

## 준비물

| | 이유 | 확인 |
|---|---|---|
| **Rust 1.95+** | 전부 | `cargo --version` |
| **Ollama** + 받아둔 모델 | 사실 추출. 기본 `qwen3-14b-nothink`은 16 GB 카드가 필요하다(실측 12.2 GB). 12 GB 카드는 Q5_K_M 양자화, 8 GB 카드에는 `qwen3:8b`(5.6 GB)가 들어가지만 추출 품질이 실측에서 크게 못 미쳤다(`docs/evidence/extraction-8b-result.md`). 실질 최소는 12 GB. GPU는 백그라운드 전용이라 남는 카드 하나면 된다. README의 *What GPU it needs* 참고 | `curl -s localhost:11434/api/tags` |
| **디스크 ~500MB** | 임베딩 모델 캐시 + SQLite 파일 | |
| **Linux** | 유일하게 테스트된 플랫폼. `File::lock()`은 이식 가능하지만 나머지는 검증 안 됨 | |

Postgres도, 벡터 DB도, Docker도 없다. sqlite-vec와 FTS5를 얹은 SQLite가 컴파일돼
들어가 있고, 임베딩·리랭킹 모델도 마찬가지다.

---

## 1. 빌드

```bash
git clone https://github.com/ohora23/memgarden
cd memgarden
cargo build --release --workspace
```

`target/release/`에 바이너리 둘이 생긴다.

- `memgardend` — 데몬
- `memgarden` — 훅 바이너리 겸 설치기

첫 빌드는 ONNX Runtime과 SQLite를 소스에서 컴파일하므로 몇 분 걸린다. 이후는
수 초.

한 번쯤 해둘 만한 것:

```bash
cargo test --workspace          # 약 700개, 네트워크 불필요
./scripts/hook-budget.sh        # 바이너리 크기, ldd 집합, 의존성 폐쇄
```

---

## 2. 설정

```bash
mkdir -p ~/.config/memgarden
cp config.example.toml ~/.config/memgarden/config.toml
```

예제 파일 자체가 문서다 — 모든 노브에 그 값을 고른 이유가 붙어 있고, 몇 개는
그 값을 결정한 실측치까지 달려 있다. 기본값이 이 시스템이 실제로 도는 값이니
3단계로 바로 넘어가도 된다.

가장 만질 일이 많은 셋:

```toml
[ollama]
model = "qwen3-14b-nothink"     # `ollama list`에 있는 것

[storage]
db_path = "~/.local/share/memgarden/memgarden.db"

[hooks]
mode = "shadow"                 # shadow | full — 사용법 참고
```

모든 값에 `MEMGARDEN_*` 환경변수 오버라이드가 있고, `MEMGARDEN_CONFIG`는 설정
파일 자체를 다른 것으로 바꾼다.

---

## 3. 데몬 실행

```bash
./target/release/memgardend
```

첫 실행에서 데이터 디렉터리를 `0700`으로 만들고, 스키마 마이그레이션을 돌리고,
임베딩 모델을 `<data>/models`로 내려받고, `127.0.0.1:9100`에서 듣는다.

```bash
curl -s localhost:9100/livez                 # ok
curl -s localhost:9100/healthz | jq .        # HEALTHY | DEGRADED | UNHEALTHY
curl -s localhost:9100/metrics.json | jq .   # 카운터, 히스토그램, 원장
```

**이 프로세스를 띄우는 건 이것뿐이다.** 훅은 데몬을 절대 실행·재시작·종료하지
않는다 — 모델을 로드하는 서비스를 훅이 띄우는 구조가 바로 이 재구축이 없애려는
재시작 경합이다. systemd든 터미널 탭이든, 다른 사용자 서비스와 같은 방식으로
관리하면 된다.

<details><summary>systemd user unit 예시</summary>

```ini
# ~/.config/systemd/user/memgardend.service
[Unit]
Description=MemGarden daemon
After=network.target

[Service]
ExecStart=%h/repositories/memgarden/target/release/memgardend
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now memgardend
```
</details>

---

## 4. 훅 연결

```bash
./target/release/memgarden hooks install --dry-run   # 먼저 본다
./target/release/memgarden hooks install             # shadow 모드
./target/release/memgarden hooks status
```

`install`은 `~/.claude/settings.json`에 **이벤트당 한 줄을 끼워 넣는다** — 파일을
다시 쓰지 않는다. 항목 넷:

| 이벤트 | 서브커맨드 | 타임아웃 | async |
|---|---|---|---|
| `SessionStart` | `hook session-start` | 5초 | — |
| `UserPromptSubmit` | `hook recall` | 10초 | — |
| `Stop` | `hook retain` | 30초 | ✔ |
| `SessionEnd` | `hook session-end` | 5초 | — |

실행 전에 알아둘 셋:

- **즉시 적용된다.** 돌고 있는 모든 Claude Code 인스턴스에서 파일 워처가 세션
  도중에 편집을 집어간다. 재시작 없다.
- **아무것도 주입하지 않는다.** 기본 모드가 `shadow`다: 훅은 돌고, 데몬은
  호출되고, 뱅크는 채워지고, 모델은 그중 아무것도 보지 않는다. 주입을 켜는 건
  `config.toml`을 고치는 별도의 명시적 단계다.
- **모든 쓰기 전에 타임스탬프 백업**을 뜨고 경로를 출력한다.

설치 전에 바이너리를 안정된 위치에 두자 — `settings.json`에 들어가는 건 절대
경로다. `cargo install --path crates/memgarden-cli`가 `~/.cargo/bin`에 넣어준다.
나중에 옮겼다면 `install`을 다시 돌린다.

---

## 5. 검증

Claude Code 세션을 열고 아무거나 입력한다. `memgarden: recalling` 상태 메시지가
보여야 하고, 그다음:

```bash
memgarden hooks status                                # 전부, 한 화면
ls ~/.local/share/memgarden/hooks/                    # 세션당 상태 파일 하나
tail -1 ~/.local/share/memgarden/hooks/shadow-recall.jsonl   # 주입했을 내용
curl -s localhost:9100/metrics.json | jq .recall_latency
```

뱅크가 차고 `shadow-recall.jsonl`이 자라면 설치는 끝이다. 무엇을 할지는
[사용법](usage.md)에 있다.

---

## 제거

노력 순서대로 — 충분한 지점에서 멈추면 된다:

```bash
export MEMGARDEN_HOOKS_DISABLE=1                    # 즉시, 셸 단위
# config.toml의 [hooks] enabled = false            # 즉시, 전역
memgarden hooks uninstall --dry-run                 # 무엇이 빠질지 확인
memgarden hooks uninstall                           # 네 줄 제거
```

`uninstall`은 자기가 넣은 줄만 지우므로 `settings.json`은 설치 전 바이트로
돌아간다. 데이터는 넷 다 살아남는다: SQLite 파일과 모든 세션 커서가 디스크에
남고, 다시 설치하면 멈춘 자리에서 이어간다.

---

## 레거시 hindsight 훅과 공존

지원되며, 전환 근거를 수집하는 방식이 바로 이것이다. `shadow` 모드로 설치하고
둘 다 연결해 둔다: 레거시가 대화를 계속 이끄는 동안 MemGarden은 자기 뱅크를
채우고 *주입했을* 내용을 기록한다.

레거시가 연결돼 있으면 `--mode full`은 **거부한다**. 레거시의 태그 제거 목록이
`<memgarden_memories>`를 모르기 때문에 우리 주입을 자기 뱅크에 적재해 버린다.
의도한 경우에만 `--allow-double-injection`을 쓴다.

전체 공존·롤백 절차는
[`docs/runbook-hooks.md`](https://github.com/ohora23/memgarden/blob/master/docs/runbook-hooks.md)에
있다.
