# Deep Interview Spec: MemGarden Rust Rebuild (PRD)

## Metadata
- Interview ID: 479fd608 | Rounds: 10 | Final Ambiguity: 13% | Type: brownfield
- Generated: 2026-08-02 | Threshold: 20% | Threshold Source: default
- Initial Context Summarized: yes (in-session system knowledge) | Status: PASSED

## Clarity Breakdown
| Dimension | Score | Weight | Weighted |
|---|---|---|---|
| Goal | 0.90 | 0.35 | 0.315 |
| Constraints | 0.90 | 0.25 | 0.225 |
| Success Criteria | 0.85 | 0.25 | 0.213 |
| Context | 0.90 | 0.15 | 0.135 |
| **Total Clarity / Ambiguity** | | | **0.887 / 13%** |

## Topology (Round 0 확정, 5 active / 0 deferred)
| Component | Status | Description | Coverage |
|---|---|---|---|
| core-engine | active | Rust 메모리 엔진 — 현행 Hindsight **완전 대체·기능 패리티** | R1,R6,R8,R9 |
| hooks-integration | active | Claude Code 훅 4종 = **Rust 바이너리 서브커맨드** | R7 |
| dashboard | active | memdash 후계 웹 대시보드 (HEALTHY 히어로+카드) | R2 + 현행 이관 |
| graph-viewer | active | **자체** 웹 그래프 뷰어 — 실시간·필터·마우스 조작 (Obsidian 참조) | R4 |
| metrics | active | 수집 무지연·무LLM 결정적 지표 + 이득 원장 | R3 |

## Goal
현행 3-저장소 체제의 자동 계층(Hindsight 데몬+Python 훅)을 **기능 패리티의 Rust 시스템 "MemGarden"** 으로 완전 대체한다: 데몬(REST) + 웹UI 분리 배포, 임베디드 SQLite 저장소, Ollama 추출 + 내장 임베딩, Rust 훅 서브커맨드, 실시간 그래프 뷰어, 오버헤드 없는 절약 지표. 기존 3개 뱅크 데이터는 무손실 이전.

## Constraints
- **완전 대체**: hindsight-embed/pg0 제거 (R1). 단 전환 게이트 통과 전까지 구시스템 가동 유지
- **배포**: `memgardend`(데몬, REST :포트 configurable)가 웹UI 정적 자산도 같은 오리진에서 서빙 — 프로세스 1개.
  R2는 원래 웹UI 서버 분리를 정했으나 E1에서 뒤집었다: 두 번째 프로세스는 이 재구축이 없애려는 프로세스
  난립을 되살리고, R2가 지키려던 "UI만 재시작"은 정적 파일이라 애초에 재시작이 필요 없어 공짜로 얻어진다.
  같은 오리진이라 `check_host`가 브라우저 기본 Host 헤더로 통과해 토큰도 CORS도 불필요.
  근거 전문: `docs/design/e1-memory-explorer.md` §Decisions 1
- **저장**: SQLite 단일 파일 + sqlite-vec(벡터) + FTS5(BM25) + 테이블 그래프 — 외부 프로세스 0 (R8)
- **LLM**: 추출=Ollama HTTP(기본 qwen3-14b, 모델 교체 가능), 임베딩·리랭커=바이너리 내장 ort/fastembed CPU (R9; 근거: GPU는 Ollama 점유, CPU 실측 4.5/12.8ms)
- **훅**: 단일 Rust CLI 서브커맨드 4종, 훅당 오버헤드 <10ms (R7)
- **지표**: recall/retain 경로에 추가 지연 0(비동기 카운터·집계), 지표 계산에 LLM 호출 금지 (R3)
- **프로세스**: 새 `ohora23/memgarden` 레포, **모든 구현은 PR 단위**, 합의된 PR 템플릿(PRD ID 추적+검증 증거) 사용 (R5)
- **패리티 이식 시 유지할 포크 개선**: tool-input 캡, retainMaxInitialMessages 백필 상한, coding profile, file: 태그, recallTypes 혼합
- **회피할 업스트림 결함**: 요청당 gc.collect류 상수비용, 재시작 경합(임베디드 저장소로 원천 제거)

## Non-Goals
- agentmemory 아카이브 이전(현행 읽기 전용 유지), 네이티브 MEMORY.md 역할 변경 없음
- 멀티유저·원격 배포·클라우드 LLM·모바일
- 병행 안정성 "기간"은 전환 게이트가 아님 (R10에서 제외)
- LLM 저지 기반 적중률 지표(Layer 3)는 v1 범위 밖
- Obsidian 연동: v1에서는 그래프 뷰어의 UX 참조 모델일 뿐 데이터 연결 없음 (EX-1 내보내기는 v2 후보)

## Acceptance Criteria (전환 게이트 = AC-1~3 전부 충족 시 구시스템 종료)
- [x] **AC-1 품질 동등성**: memcompare 방식으로 동일 질의 세트(기존 A/B 로그 8건+신규 12건) 회수 품질이 현행 대비 동등 이상 (사용자 판정)
  — **사용자 서명 2026-08-20.** 배포 설정(mid/1024/세 타입)에서 블라인드 패널 판정: 출하 구성 기준 **우세 13 / 열세 5 / 동등 1**, 판정 불가 0. 게이트 조건(`열세 ≤ 우세`) 성립.
  근거: `docs/evidence/ac-1-blind-panel.md`(방법·한계), `ac-1-criteria.md`(사전 커밋된 기준), `ac-1-memcompare.md`(2026-08-12 최초 실행, 손잡이 결함으로 폐기됨)
- [x] **AC-2 성능**: recall p50 ≤35ms·p95 ≤60ms(로컬), 훅 총 오버헤드 <10ms, retain 캡 절감률(-55~87%) 유지 — 훅 0.845ms/턴 실측(레거시는 비활성 경로에서 33ms)
- [x] **AC-3 무손실 마이그레이션**: 기존 3개 뱅크 노드·링크·문서 카운트 일치 + 무작위 샘플 50건 내용 대조 통과(검증 스크립트) — `mg-migrate verify` exit 0, 라이브 DB는 2026-08-08 이전 완료. `docs/evidence/ac-3.json`
- [x] AC-4 그래프 뷰어: **≥2,500노드는 렌더링 벤치마크**(사용자가 그만큼 펼쳤을 때 팬/줌/드래그/호버가 원활),
  기본 화면이 아니다 — CE-7 수정 후 라이브 뱅크는 5,333노드·167,398링크라 전부 그리면 느려지기 전에 읽을 수
  없다. 뷰어는 필터된 부분집합으로 열고 클릭한 노드의 이웃을 덧붙인다. retain 반영 ≤5s, 세션·뱅크·기억타입 필터
  — 2026-08-19 실측: 3,200노드·57,890엣지에서 pan+zoom **p50 3.6ms / p95 5.3ms**(60fps 예산 16.7ms 대비 3.2배 여유), hover 히트테스트 p95 0.40ms. `docs/evidence/ac-4-render.md`
- [x] AC-5 대시보드: memdash 전 기능 + HEALTHY/DEGRADED/UNHEALTHY 판정, 10s 자동 갱신 — E5(`/ui/dashboard`)
- [x] AC-6 지표: 이득 원장·Layer1/2 자동 수집, 수집 경로 지연 증가 0 실측 증명 — MX-1 카운터 + E5 원장 뷰
  — **범위 결정 2026-08-20:** AC-1은 *훅이 자동 캡처한 기억의 회수 품질*만 잰다. 결론형 질문의 답은 큐레이션된 `MEMORY.md`에 있고 두 시스템 어느 쪽도 그것을 캡처하지 않으므로 이 게이트의 대상이 아니다(`ac-1-shadow.md` 미결 사항 종결).
- [x] AC-7 전 PR이 템플릿 준수(PRD ID + 검증 증거), `cargo test` 통과
  — **템플릿 조항 사용자 서명 2026-08-26 (충족).** 템플릿은 **#14에 도입되어 #14~#27 전건이 예외 없이 준수**했다.
  #1~#13은 도입 이전 PR이며, 13건 중 12건이 PRD ID·실측 증거를 산문으로 담고 있으나 템플릿 표제는 따르지 않는다
  (순수 미달은 #5 `actions/checkout` v4→v5 하나). **머지된 PR 본문을 소급 편집해 통과시키지 않았다** — 일어나지 않은
  과정을 기록으로 남기는 일이고, AC-1 최초 측정·64배 링크 주장·CPU-3 결론·골드 하네스를 철회해 온 규칙과 어긋난다.
  — **`cargo test` 조항 사용자 서명 2026-08-26 (충족).** 커널 `-30`, uptime 22h43m에서 **워크스페이스 20회 연속 통과·
  SIGSEGV 0·커널 경고 0**, 새 크래시 덤프 없음, 최종 집계 **867 passed / 0 failed (33 suites)**. 같은 날 `-29`에서는
  4회 중 2회 사망했다.
  **기준선을 31시간에서 12시간으로 낮춘 것은 사용자 결정이며 약화이므로 명시한다** — 패닉은 1.7·13·31시간에 났고
  12시간은 그중 둘만 넘긴다. 또한 `cpu3`/`cpu11`이 오프라인이라 조용한 이유가 커널인지 코어 제거인지 **가리지 못한다.**
  AC-7은 "테스트가 통과하는가"를 묻고 그건 충족됐다. "왜 크래시했는가"는 CPU-3 대조 실험으로 분리해 열어 둔다.
  — 2026-08-26 02:07 사용자가 `cpu3`/`cpu11`을 예방 차원에서 오프라인 처리(`nproc` 16→14, 재부팅 시 해제).
  판정은 비대칭이 된다: 조용하면 여전히 미결(커널 수정과 코어 제거가 구분 안 됨), **패닉이 나면 CPU-3 가설 기각.**
  근거: `docs/evidence/ac-7.md`

### v1 이후 대기 중인 측정
- **~~CPU-3 대조 실험~~ 완료 (2026-08-28) — 가설 기각.** 두 팔 모두 커널 `-30`에서:
  treatment(코어 오프라인·14스레드) **43h55m 무사고**, control(**전체 16스레드**) **25h27m 무사고**,
  양쪽 신규 패닉 0·커널 경고 0. **용의자 코어가 25시간 돌았는데 아무 일도 없었다.**
  CPU 3은 폴트가 **착지한** 곳이지 원인이 아니었다 — 스케줄러·FPU 상태를 망가뜨린 커널은
  잔해를 쥔 CPU에서 터진다. 코어는 계속 켜 둔다. `-29`가 최유력이지만(패닉 전건이 거기, `-30`은
  69시간 0건) 노출량은 대등하지 않다(control 25h < 최장 관측 간격 31h, 사용자 판단으로 종료).
  `docs/evidence/ac-7.md`

- **~~`include_tool_calls` A/B~~ — 전제 오류로 폐기 (2026-08-27).** 라이브는 이미 `false`다
  (`profile.name = ""`, 프리셋 미적용). `coding` 프로필이 켜 뒀다는 서술은 틀렸고, 뱅크에 저장된
  mission 문자열(08-08 마이그레이션 산물)을 현재 설정으로 오독한 것이다.
  유입 시기별로 가르면 **네이티브 retain(08-23~)은 676노드 중 2건**이고 그 2건도 분류기 오탐이다 —
  신규 오염은 이미 멎었다. 텍스트만 넣어도 어시스턴트 산문에서 명령 로그가 생기므로(08-21 캐치업 6.4%)
  손잡이로 막을 수 있는 경로도 아니었다.
- **~~명령 로그 178건 정리~~ 완료 (2026-08-27).** 178건 전량을 읽어 **20건 보존·158건 삭제**.
  **주입 내 명령 로그 비율 22% → 0.9%** (동일 프롬프트 6건 재현). 노드 7,330 → 7,172.
  분류기 정밀도는 12건 표본 추정 92%였으나 전수 확인 결과 **89%** — 오탐 20건 중에는 Hindsight
  830ms→20ms 규명, 훅 구현 결정, 반영 게이트 설명이 들어 있었다. **표본이 틀린 게 아니라 무엇을
  잃을지 보기엔 작았다.** `integrity_check ok`·FK 위반 0·고아 0·FTS 일치·데몬 HEALTHY.
  `docs/evidence/command-log-cleanup.md`

## Assumptions Exposed & Resolved
| 가정 | 검증 방법 | 결론 |
|---|---|---|
| "Rust로 재구축" = 전체 대체인가 | R1 직접 질문 | 완전 대체 확정 |
| 그래프 뷰어 자체 구축이 필요한가 | R4 Contrarian(Obsidian 대체안) | 필수 — 실시간·필터 요구 |
| "오버헤드 없이"의 의미 | R3 분해 질문 | 수집 무지연 + 무LLM |
| 미니멀이 낫지 않나 | R6 Simplifier + 실측 견적(103K LOC 분석) | 견적 확인 후 패리티 선택 |
| 훅은 Python 유지? | R7 | Rust 서브커맨드 |

## Technical Context
- 참조 구현: hindsight-api 0.8.6 = 103,555 LOC Python; 우리-패리티 실질 대상 ≈40~45K LOC 상당 (프로바이더 매트릭스·Oracle·alembic·멀티테넌트 제외)
- 예상 규모: **65~85 PR, 6~9주** (Claude 협업 페이스 기준)
- 현행 자산 재활용: 추출·컨솔리데이션 프롬프트(포팅), memcompare 질의 로그, memdash 지표 정의, WebGL 뷰어 경험(1차 memdash_web에서 검증됨), 포크 패치 6종의 동작 명세

## Ontology (5라운드 연속 안정도 100%)
| Entity | Type | Fields | Relationships |
|---|---|---|---|
| MemoryEngine | core | extraction, storage, recall, consolidation | replaces Hindsight daemon |
| Bank | core | project-scoped, mission | Engine has many Banks |
| MemoryNode | core | text, type(world/obs/exp), embeddings, temporal | Bank has many; Node links Node |
| Hook | supporting | 4 events, turn state, retention progress | feeds Engine |
| LocalLLM | external | Ollama HTTP, swappable model | Engine uses for extraction |
| Dashboard | supporting | health hero, cards, 10s refresh | reads Engine + Metrics |
| GraphViewer | supporting | pan/zoom/drag/hover, filters, realtime | renders Nodes+Links |
| BenefitLedger | supporting | counters, deterministic, zero-latency | measures Engine |

## Work Order (PRD Item IDs — PR은 이 ID를 참조)
**Phase A. 기반 (CE-1~3, MX-1)**: 워크스페이스·CI → SQLite 스키마(vec+FTS5+graph+temporal) → REST 골격+설정 → 지표 카운터 배관(처음부터 무지연 설계)
**Phase B. 코어 파이프라인 (CE-4~11)**: 내장 임베딩 → retain 추출(Ollama, 캡 2종 포함) → 하이브리드 recall(BM25+벡터+RRF+예산) → 엔티티/링크+그래프 검색 → temporal → 컨솔리데이션 → reflect/mental models → 내장 리랭커
**Phase C. 훅 (HK-1~2)**: 서브커맨드 4종(+턴/retention 상태) → 글로벌 settings 전환 스위치
**Phase D. 마이그레이션 (MG-1~2)**: pg→SQLite 익스포터/임포터 → AC-3 검증 스크립트
**Phase E. UI·지표 (DB-1, GV-1~3, MX-2)**: 대시보드 → 그래프 API → WebGL 뷰어(마우스 조작) → 실시간(SSE)+필터 → 지표·원장 뷰
**Phase F. 전환**: AC-1~3 게이트 실행 → 구시스템 종료 → legacy 레포에 최종 기록 — ✅ **2026-08-21 완료.** 게이트 서명(08-20) → 미이전 2뱅크 811노드 구제 → 훅 제거·데몬 종료 → [최종 기록](https://github.com/ohora23/memgarden-legacy). 경과는 `docs/evidence/cutover.md`

**v2 후보 (v1 범위 밖, 착수 금지)**: EX-1 Obsidian vault 내보내기(MemoryNode→마크다운+백링크, Library vault 합류 — 뷰어 대체가 아닌 보관·열람 보완), 컨솔리데이션 고도화, LLM 저지 적중률 지표(Layer 3)

의존성: A→B→(C,D,E 병렬)→F. 그래프 뷰어(GV)는 CE-7(링크 데이터) 이후 착수 가능.

## Interview Transcript
<details><summary>10 rounds 요약</summary>

R0 토폴로지: 4→5개(훅 분리, 사용자 수정) | R1 완전 대체 | R2 데몬+웹UI 분리 | R3 지표=수집무지연+무LLM | R4(Contrarian) 자체 뷰어 필수 | R5(사용자 주도) 레포 분리+PR 워크플로+템플릿 | R6(Simplifier) 실측 견적 후 현행 패리티 | R7 Rust 훅 서브커맨드 | R8 SQLite 스택 | R9 Ollama+내장 임베딩 | R10 게이트=품질동등+성능+무손실
</details>
