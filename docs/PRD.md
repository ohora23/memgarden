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
- [ ] **AC-1 품질 동등성**: memcompare 방식으로 동일 질의 세트(기존 A/B 로그 8건+신규 12건) 회수 품질이 현행 대비 동등 이상 (사용자 판정)
  — 2026-08-12 실행: 우세 6 / 동등 2 / 열세 5 / 판정불가 7, 조건 성립(6>5)이나 **사용자 서명 대기**. `docs/evidence/ac-1-{criteria,memcompare,shadow,ranking-attempt}.md`
- [x] **AC-2 성능**: recall p50 ≤35ms·p95 ≤60ms(로컬), 훅 총 오버헤드 <10ms, retain 캡 절감률(-55~87%) 유지 — 훅 0.845ms/턴 실측(레거시는 비활성 경로에서 33ms)
- [x] **AC-3 무손실 마이그레이션**: 기존 3개 뱅크 노드·링크·문서 카운트 일치 + 무작위 샘플 50건 내용 대조 통과(검증 스크립트) — `mg-migrate verify` exit 0, 라이브 DB는 2026-08-08 이전 완료. `docs/evidence/ac-3.json`
- [ ] AC-4 그래프 뷰어: **≥2,500노드는 렌더링 벤치마크**(사용자가 그만큼 펼쳤을 때 팬/줌/드래그/호버가 원활),
  기본 화면이 아니다 — CE-7 수정 후 라이브 뱅크는 5,333노드·167,398링크라 전부 그리면 느려지기 전에 읽을 수
  없다. 뷰어는 필터된 부분집합으로 열고 클릭한 노드의 이웃을 덧붙인다. retain 반영 ≤5s, 세션·뱅크·기억타입 필터
  — E1~E4로 기능은 전부 들어왔으나(필터·점진 확장·SSE) **≥2,500노드 렌더링 벤치마크 수치가 기록되지 않았다**
- [x] AC-5 대시보드: memdash 전 기능 + HEALTHY/DEGRADED/UNHEALTHY 판정, 10s 자동 갱신 — E5(`/ui/dashboard`)
- [x] AC-6 지표: 이득 원장·Layer1/2 자동 수집, 수집 경로 지연 증가 0 실측 증명 — MX-1 카운터 + E5 원장 뷰
- [ ] AC-7 전 PR이 템플릿 준수(PRD ID + 검증 증거), `cargo test` 통과

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
**Phase F. 전환**: AC-1~3 게이트 실행 → 구시스템 종료 → legacy 레포에 최종 기록

**v2 후보 (v1 범위 밖, 착수 금지)**: EX-1 Obsidian vault 내보내기(MemoryNode→마크다운+백링크, Library vault 합류 — 뷰어 대체가 아닌 보관·열람 보완), 컨솔리데이션 고도화, LLM 저지 적중률 지표(Layer 3)

의존성: A→B→(C,D,E 병렬)→F. 그래프 뷰어(GV)는 CE-7(링크 데이터) 이후 착수 가능.

## Interview Transcript
<details><summary>10 rounds 요약</summary>

R0 토폴로지: 4→5개(훅 분리, 사용자 수정) | R1 완전 대체 | R2 데몬+웹UI 분리 | R3 지표=수집무지연+무LLM | R4(Contrarian) 자체 뷰어 필수 | R5(사용자 주도) 레포 분리+PR 워크플로+템플릿 | R6(Simplifier) 실측 견적 후 현행 패리티 | R7 Rust 훅 서브커맨드 | R8 SQLite 스택 | R9 Ollama+내장 임베딩 | R10 게이트=품질동등+성능+무손실
</details>
