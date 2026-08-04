# 확장하기

> 🇬🇧 [English](../extending.md)

이음매가 어디 있고, 여는 데 얼마가 들고, 여는 동안 시스템을 정직하게 유지하는
규칙 둘.

---

## 설정부터 본다

"이거 되나요"의 대부분은 이미 TOML 키다. `config.example.toml`이 레퍼런스이고,
모든 항목에 그 값을 고른 이유가 붙어 있다 — 몇 개는 그 값을 결정한 측정치까지.

| 하고 싶은 것 | 노브 |
|---|---|
| 다른 추출 모델 | `[ollama] model` — `ollama list`에 있는 아무거나 |
| 리랭커 켜기 | `[reranker] enabled = true` (기본 꺼짐, 아래 참고) |
| 적재 주기 조정 | `[hooks] retain_every_n_turns` |
| 회수 예산 늘리기 | `[hooks] max_inject_bytes`, 그리고 요청 자체의 `limit` |
| 디렉터리별 뱅크 | `[hooks] directory_bank_map` |
| 통합을 더 세게 | `[consolidation] batch_size`와 스케줄 키들 |
| 지금 당장 전부 끄기 | `MEMGARDEN_HOOKS_DISABLE=1` |

모든 키에 `MEMGARDEN_*` 환경변수 오버라이드가 있다. 공유 파일을 고치지 않고 한
세션만 실험하는 올바른 방법이다.

---

## 코드가 들어가는 세 자리

### 새 REST 엔드포인트

`crates/memgardend/src/routes/` — 리소스당 모듈 하나, `routes/mod.rs::router`에
등록. 규약은 선택이 아니다:

- **모든 쓰기는 `Db::write`를 통과**하고, `rusqlite` 핸들이 `await`를 넘지 않는다;
- 응답은 타입 있는 오류를 가진 `Json<T>`, 맨 문자열 금지;
- 비싸면 `POST`이고 호출자 타임아웃을 받는다. `GET` 뒤의 동기 LLM 작업이 바로
  대시보드가 멈추는 방식이다.

`check_host`와 `stamp_token`은 모든 라우트에 자동 적용된다. 옵트인하는 게 아니고,
옵트아웃해서도 안 된다.

### 새 훅 서브커맨드

`crates/memgarden-cli/src/cmd/`와 `lib.rs::dispatch`의 팔 하나.
[동작 원리](design.md#훅-바이너리)를 먼저 읽고, 다음을 지킨다:

- **절대 exit 2를 낼 수 없다.** `clap` 금지, `main` 밖으로 `?` 금지, 0이 아닌
  코드를 반환하는 경로 금지;
- **stdout은 프로토콜이다.** `UserPromptSubmit`과 `SessionStart`에서 stdout은
  모델의 컨텍스트 채널이라, 출력하는 것이 대화에 들어간다. 그 외에는 아무것도
  출력하지 않는다;
- 상태는 락 아래 세션별 파일에 넣고, 다른 훅도 잡는 락을 잡는다면
  **`with_try_lock`을 쓴다** — [동작 원리](design.md#락-교훈) 참고;
- 의존성 폐쇄는 CI로 강제된다. `use memgarden_store::…`는 잘 컴파일되면서 세션당
  수천 번 도는 프로세스에 1.5MB의 SQLite를 조용히 얹는다.

그리고 페어드로 측정해서 PR에 숫자를 넣는다:

```bash
cargo build --release -p memgarden-cli --bins
./target/release/hook_bench --arm-a "hook yours" --stdin-a payload.json --n 300
```

### 파이프라인의 새 단계

`crates/memgardend/src/retain/`(적재) 또는 `recall/`(회수). 적재 파이프라인은
캡 → 청킹 → 추출 → 사실+엔티티+링크 → 임베딩. 회수는 질의 분석 → FTS5 BM25 +
`sqlite-vec` KNN → RRF 융합 → 선택적 리랭크 → 토큰 예산.

**랭킹을 건드리면 골드 하네스를 돌린다.** 랭킹 변경은 델타를 보고하거나, 머지되지
않는다:

```bash
# 코퍼스를 한 번 적재한 뒤 등급 질의로 측정
recall_bench import gold/corpus.jsonl <db-path>
recall_bench bench  <db-path> gold/queries.jsonl gold/corpus.jsonl results.jsonl
```

`bench`는 DB 노드 수가 코퍼스 줄 수와 다르면 실행을 거부한다 — "엉뚱한 DB를
벤치했다"가 품질 회귀와 똑같이 생겼기 때문이다. `now_ms`는 고정돼 있어서 기준선이
매일 떠내려가지 않는다.

이건 형식이 아니다. 내장 리랭커는 시간 처리 버그가 고쳐지기 전까지 명백한 승리로
보였고, 고친 뒤 같은 측정이 recall@10을 *잃는다*고 말했다. 그래서 기본이 꺼짐이다.

---

## 일부러 뺀 것들과, 들어올 조건

`docs/parity-gaps.md`가 그 목록이고, 모든 행에 **재진입 기준** — 참이 되어야 할
구체적 사실 — 이 적혀 있다. 알아둘 만한 몇 개:

| 빠진 것 | 무엇이 열어주는가 |
|---|---|
| 리랭커 기본 켜기 | +14ms p50을 흡수할 수 있는 호출자, **또는** 적재 루프를 굶기지 않는 리랭크 경로 |
| 교차 뱅크 회수 | 프로젝트 뱅크와 함께 회수할 가치가 있는 공용 사용자 프로필 뱅크 |
| 다중 턴 질의 구성 | 골드 세트에서 다중 턴이 단일 턴을 이긴다는 AX-2 실행 결과 |
| 데몬 수명 관리 | 마이그레이션과 경합할 수 없는 기동 경로 |
| reflect 에이전틱 도구 루프 | 복구용 비계 없이 10턴을 버티는 도구 호출 모델 |

기준은 양방향으로 반증 가능하게 쓰여 있다. 충족은 만들 이유이고, 미충족은 논의를
닫을 이유다 — 이 파일의 용도 대부분이 후자다.

---

## 두 가지 규칙

### 모든 변경은 근거와 함께 PR로

템플릿(`.github/pull_request_template.md`)이 PRD 항목 id, 테스트 수, 관찰된
출력이 붙은 수동 확인 1건, `Measured:` 줄을 요구한다. 지연에 민감한 경로를
바꾸면서 숫자를 보고하지 않는 PR은 미완이다 — 취향이 아니라 명시된 규칙이다.

각 PR은 `docs/design/<id>-<slug>.md`에 `## Diverged from legacy` 절을 가진 설계
노트를 함께 낸다. 노트는 diff 없이도 홀로 서도록 쓴다. 6개월 뒤 아무도 다시 읽지
않는 게 diff니까.

### 페어드로 측정하고, 런을 가로질러 비교하지 않는다

이 하드웨어에서 절대값의 런 간 비교는 무효다 — 동일 커밋을 다시 벤치했더니
**동일 비트에서 +1.5ms**가 나왔다. 그래서:

- 훅은 `hook_bench`를 쓴다. 하나의 드라이버 프로세스가 `A,B,A,B…`로 번갈아
  돌리고 arm B는 `hook noop`이며 `A_i − B_i`를 보고한다;
- 데몬 측 수치는 `/metrics.json`의 **정확한** `under_35ms` / `under_60ms`
  카운트에서 가져온다. 보간된 백분위가 아니라;
- "바이너리가 커졌으니 느려졌다"는 결과가 아니라 가설이다. 두 빌드를 페어링해서
  (`hook_bench --bin-b <old>`) 확인한다 — 이 대조는 이미 "메커니즘은 맞고 크기는
  5배 틀린" 귀속을 한 번 잡아냈다.

---

## 테스트 규약

- **테스트 이름은 무엇이 깨지는지를 말하는 문장이다**:
  `a_poisoned_at_in_the_future_does_not_throttle_a_session_out_of_existence`.
- **구별 가능한 값 둘, 하나 말고.** `150ms <= elapsed < 600ms`를 주장하는
  타임아웃 테스트는 그것이 잡으려던 하드코딩된 400ms에서 통과한다.
- **사용자의 실제 파일은 테스트가 절대 쓰지 않는다.** `~/.claude/settings.json`은
  금지. 모든 설정 테스트가 `--settings <임시파일>`을 넘기고 그 위에 `HOME`까지
  돌린다.
- **레거시는 건드리지 않는다.** hindsight 데몬을 재시작하지 않고, 9077·9090을
  바인드하지 않는다. 테스트 리스너는 포트 0을 쓴다.
- **인메모리 SQLite는 shared-cache**라서 두 커넥션이 한 테이블에 쓰면
  `SQLITE_LOCKED`가 나고, 이건 `busy_timeout`이 재시도하지 않는다. 동시성
  테스트에는 `tempfile` + 실제 `Db::open`을 쓴다.

---

## 하나씩 돌리기에 대한 경고

훅 단계에서 가장 날카로웠던 버그는 **모든 테스트가 훅을 하나씩 돌렸기 때문에**
스위트 전체에 보이지 않았다: 적재가 네트워크 너머로 락을 쥐고, 회수가 매
프롬프트마다 같은 락을 잡았고, 그걸 되돌리는 변이가 전체 스위트를 통과했다.

당신의 변경이 두 훅에게 무언가를 — 락, 파일, 소켓을 — 공유시킨다면, 회귀를 잡을
테스트는 **프로세스를 둘** 돌려야 한다. 그것이 이 하네스의 기본 맹점이고, 초록불이
곧 안전이라고 가정하기 전에 다시 읽을 값어치가 있다.
