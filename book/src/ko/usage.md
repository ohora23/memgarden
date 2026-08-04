# 사용법

> 🇬🇧 [English](../usage.md)

설치가 끝나면 MemGarden은 안 보이는 게 정상이다. 이 페이지는 그래도 들여다보고
싶을 때 쓰는 것들이다.

---

## 두 개의 스위치

의도적으로 독립돼 있고, 지금 어느 쪽을 만지는지 아는 것이 이 시스템 운영의
대부분이다.

| | 위치 | 결정하는 것 |
|---|---|---|
| **배선** | `~/.claude/settings.json` | Claude Code가 훅을 실행하는지 여부 |
| **런타임** | `~/.config/memgarden/config.toml` | `enabled`, 그리고 `mode = shadow \| full` |

설치는 첫 번째를 정한다. 두 번째는 절대 건드리지 않는다 — `hooks install --mode
full`은 바꿔야 할 줄을 출력만 하고 쓰지 않는다. 스위치를 설치하는 행위가 스위치를
넘길 수 없도록.

### shadow와 full

| | `shadow` (기본) | `full` |
|---|---|---|
| session-start, retain, session-end | 라이브 — 뱅크가 찬다 | 라이브 |
| recall | 데몬 호출, 실제 지연, `shadow-recall.jsonl`에 기록 | `additionalContext` 출력 |
| 모델이 보는 것 | **없음** | MemGarden의 기억 |

shadow는 예행연습이 아니다. 모든 코드 경로를 실제로 태우고 전환 판정용 A/B
근거를 만든다. 유일하게 보류하는 건 주입 그 자체다.

라이브로 가려면 한 줄:

```toml
[hooks]
mode = "full"
```

재시작 없다. 다음 프롬프트가 읽는다.

---

## 일상

```bash
memgarden hooks status
```

이 한 줄만 기억하면 된다. 순서대로 보고한다:

- 해석된 설정, 런타임 모드, `MEMGARDEN_HOOKS_DISABLE`이 덮고 있는지 여부
- **이벤트별로** 어느 메모리 시스템이 연결돼 있는지 — MemGarden, 레거시, 또는 없음
- `memgardend`의 `/livez`·`/healthz`, 그리고 레거시 데몬 기동 여부
- 세션 상태: 개수, 가장 오래된 것, 오염(poisoned)된 것
- **`unconfirmed`** — 이 머신이 보냈지만 적재 확인이 안 된 바이트
- 두 시스템이 동시에 연결됐을 때만 의미 있는 경고(GPU 경합, `full`이면 이중 주입)

항상 0으로 끝난다. 게이트가 아니라 진단이다.

### 훅이 실제로 하는 일

| 이벤트 | 하는 일 | 비용 |
|---|---|---|
| `SessionStart` | 뱅크·세션 행 upsert, 정체된 세션 따라잡기와 상태 파일 정리를 하는 분리 자식 프로세스 생성 | 0.55ms |
| `UserPromptSubmit` | 뱅크 대상 회수 1회, 400ms 상한, 전송 실패 3회면 서킷 브레이커 | 0.47ms |
| `Stop` | 10턴마다: 바이트 오프셋으로 트랜스크립트 델타를 읽어 POST | 0.38ms (게이트된 턴) |
| `SessionEnd` | 마지막 retain을 분리 자식으로 띄우고 종료 | 0.36ms |

retain은 매 턴이 아니라 `Stop` 10회마다 발동하고(`retain_every_n_turns`), 세션의
첫 retain은 트랜스크립트 전체를 보낸다 — 예산을 알면서 넘기는 유일한 지점이고,
그래서 `Stop` 항목이 `async`다.

---

## 시스템 읽기

```bash
curl -s localhost:9100/healthz | jq .              # HEALTHY / DEGRADED / UNHEALTHY
curl -s localhost:9100/metrics.json | jq .         # 카운터, 지연 히스토그램, 원장
curl -s localhost:9100/v1/banks | jq .             # 무엇이 있는지
curl -s "localhost:9100/v1/banks/<bank>/sessions" | jq .
```

뱅크에 직접 물어보기:

```bash
curl -s -X POST localhost:9100/v1/banks/<bank>/recall \
  -H 'content-type: application/json' \
  -d '{"query": "커서 프로토콜은 어떻게 정했더라?", "limit": 8}' | jq .
```

뱅크 id는 `claude-code::<project>` 형태라 `::`를 포함한다. **URL 경로에서는
퍼센트 인코딩**할 것: `claude-code%3A%3Amemgarden`.

`/metrics.json` 백분위에 대해 두 가지: 그것은 **20개 고정 버킷 안의 선형
보간**이므로 훅 벤치의 정확한 순서통계량과 절대 비교하면 안 된다. 반면
`under_35ms` / `under_60ms` 카운트는 **정확하다** — 그 경계가 곧 SLO 경계이기
때문이다.

---

## 이상해 보일 때

| 증상 | 먼저 볼 것 |
|---|---|
| 뱅크에 아무것도 안 쌓임 | `hooks status` — 데몬이 떠 있나, `enabled`가 true인가? |
| 상태 메시지가 안 뜸 | `settings.json`의 경로에 바이너리가 아직 있나? 옮겼으면 `install` 재실행 |
| 훅이 아무것도 안 하는 것 같음 | `[hooks] debug = true`가 호출당 한 줄을 **stderr**에 남긴다. 종료 코드는 절대 못 바꾼다 |
| 프롬프트가 느려짐 | 회수는 400ms에서 열린 채 실패하고 전송 실패 3회면 브레이커가 열린다 — 데몬이 죽었는지는 `hooks status`가 말해준다 |
| `unconfirmed`가 계속 증가 | retain이 큐에만 쌓이고 완료되지 않는 중. `retain_jobs` 적체와 Ollama 경합을 본다 |
| 특정 세션이 retain을 멈춤 | 오염됐을 수 있다: `hooks status`가 나열하고 `--clear-poison <sid>`가 푼다 |
| settings.json이 이상함 | 타임스탬프 백업이 `~/.local/share/memgarden/hooks/`에 있다 |

버그가 아니라 **설계**인 실패 모드 둘:

- **회수는 열린 채 실패한다(fail open).** 데몬이 없든, 타임아웃이든, 브레이커가
  열렸든 — 턴은 기억 없이 그냥 진행된다. 메모리 계층이 프롬프트 실패의 원인이
  되어서는 안 된다.
- **적재는 닫힌 채 실패한다(fail closed).** 실패한 retain은 델타를 버리지 않는다.
  트랜스크립트 자체가 스풀이라, 다음 `Stop`이나 `SessionEnd` 자식, 그것도 아니면
  다음 세션의 catch-up이 같은 파일에서 다시 보낸다.

---

## 오염된 세션

데몬이 **지속적으로** 거부한 세션(연속 4xx 10회)은 오염 상태가 된다: 매 턴이
아니라 한 시간에 한 번 재시도한다. 래치가 아니라 느린 재시도 상태이고, 한 번만
성공하면 풀린다.

```bash
memgarden hooks status                              # 목록
memgarden hooks status --clear-poison <session-id>  # 스탬프와 카운터를 함께 해제
```

---

## 백업

```bash
cp ~/.local/share/memgarden/memgarden.db /어딘가/안전한곳/
```

이게 백업의 전부다. WAL 모드 SQLite 파일 하나. 두 번째 저장소도, 상태를 쥔 외부
프로세스도, 내보내기 단계도 없다.
