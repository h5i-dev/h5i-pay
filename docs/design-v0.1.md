# h5i-app Payment Infrastructure：設計案 v0.1

h5i-app を使って、GPU-accelerated batch processing と Lean 4-verified financial logic を備えた、オープンソースの決済基盤を作る。

Stripe の決済ネットワーク自体を再実装するのではなく、Hyperswitch、TigerBeetle、Formance を参考に、次の3つを中核に据える。

- Payment orchestration：Stripe などの決済プロバイダーへの接続、決済状態管理、refund、retry。
- Financial ledger：複式簿記、残高管理、資金移動、idempotency、監査ログ。
- GPU batch processing：大量の決済データの検証、reconciliation、routing、集計。

Agent Payments にも対応できる設計にするが、最初から agent 専用にはしない。一般的な決済・請求・送金 API を提供し、その一つのユースケースとして agent による決済を扱う。

重要なのは、GPU はまず大量データの処理に使い、実際のお金の移動と永続化は CPU + transactional storage に任せること。

## 1. 作るべき機能

| 機能                      | 内容                           | 優先度 |
| ----------------------- | ---------------------------- | --- |
| Payment API             | Create、capture、cancel、refund | P0  |
| Provider Connector      | Stripe、Mock PSP              | P0  |
| Double-entry Ledger     | Accounts、postings、balances   | P0  |
| Payment State Machine   | 正当な状態遷移、retry                | P0  |
| Idempotency             | 重複リクエストの安全な処理                | P0  |
| Batch API               | 大量の決済・台帳操作                   | P0  |
| Reconciliation          | 外部明細と内部記録の照合                 | P0  |
| GPU Batch Engine        | CPU/GPU 共通のバッチ計算             | P0  |
| Lean 4 Verification     | Ledger と決済ロジックの証明            | P0  |
| Webhooks                | 署名検証、重複処理、再送                 | P0  |
| Dashboard               | 取引・残高・照合結果の表示                | P1  |
| Multi-PSP Routing       | 手数料、成功率、制約による選択              | P1  |
| Settlement / Payout     | 送金・精算管理                      | P1  |
| Disputes                | Chargeback、異議申立て             | P1  |
| Agent Spending Policies | 予算、委譲権限、利用制限                 | P1  |
| Subscription Billing    | 定期請求、従量課金                    | P2  |
| Fraud Scoring           | GPU を使ったリスク分析                | P2  |
| Multi-currency / FX     | 為替、通貨別残高                     | P2  |

P0 では Stripe の test mode と Mock PSP に限定する。実際のカード番号をシステムに取り込まず、プロバイダーの token / hosted checkout を使う。

## 2. 参考にする OSS

- Hyperswitch (https://github.com/juspay/hyperswitch): Payment lifecycle、provider connectors、routing、refund、retries。Rust の connector library `prism` と standalone decision engine も参考になる。
- TigerBeetle (https://github.com/tigerbeetle/tigerbeetle): Atomic transfers、idempotency、batching、write-ahead log、deterministic state machine。金融システムとしての correctness の参考に最も重要。
- Formance Ledger (https://github.com/formancehq/ledger): Programmable ledger、Numscript、batch operations、request signing、監査チェーン。金融ロジックの DSL と deterministic execution が参考になる。
- Blnk (https://github.com/blnkfinance/blnk): PostgreSQL ベースの ledger、wallets、inflight transactions、reconciliation。MVP の API と DB モデルを設計する際に参考にしたい。

Hyperswitch の巨大な connector catalog を再実装する必要はないし、TigerBeetle の consensus protocol をゼロから作る必要もない。最初は h5i-app + PostgreSQL で独立した正確な ledger を作り、性能限界が明確になってから専用 ledger storage を検討したい。

## 3. GPU batch processing：何を高速化するか

TigerBeetle の高スループットの中心は CPU 上の deterministic state machine、ストレージ I/O の事前取得、複数レイヤーの batching であり、依存する transfer の commit は逐次実行する。決済件数が多いから GPU 化すれば速くなるわけではない。GPU が得意な independent batch computation と、順序・排他制御が必要な financial state mutation を分離する。

| 処理                               | GPU 期待値  | 難易度   | 最初の対象 |
| -------------------------------- | -------- | ----- | ----- |
| Reconciliation / matching        | 非常に高い    | 中     | ◎     |
| Batch policy validation          | 高        | 低〜中   | ◎     |
| Transaction risk scoring         | 高        | 中     | ○     |
| Batch fee / routing evaluation   | 中〜高      | 中     | ○     |
| Ledger aggregation               | 中〜高      | 中     | ○     |
| Cryptographic batch verification | アルゴリズム依存 | 高     | 後回し   |
| Ledger commit                    | 低〜不確実    | 非常に高い | CPU   |
| Individual payment API           | 低        | 低     | CPU   |

### GPU Kernel A：Batch reconciliation（最優先）

内部 ledger 100万件と PSP settlement records 100万件を照合し、一致・不一致・欠落・重複・金額/通貨の相違を検出する。GPU では join、group-by、key comparison、reduction を行う。出力は Matched（Exact/valid）、Mismatch（Amount/status）、Unmatched（Missing/extra）。差分から ledger を修正する処理は独立した承認・監査フローにし、GPU の結果で直接資金を動かさない。CPU hash join も強いので、GPU が勝つ件数を測定する。

### GPU Kernel B：Batch policy validation

10万件の支払いリクエストについて amount/currency/recipient の検証、tenant/merchant allowlist、取引ごとの限度額、transaction type ごとの制約を並列評価する。Rust の pure function として validation kernel を定義し、CPU と GPU で同じ結果を返す。残高・累計予算など共有状態に依存する条件は GPU だけで確定しない。最終承認には DB 上の atomic reservation / commit が必要。

### CPU と GPU の比較ベンチマーク

- Workload: Reconciliation、Policy validation、Ledger aggregation、Routing evaluation
- CPU baseline: Single-thread、Multithread、SIMD
- Input sizes: 100K / 1M / 10M records
- 測定指標: throughput、p50/p95 latency、GPU transfer time、peak memory、cost per million records、CPU/GPU result equivalence

最初から GPU の効果を宣伝せず、CPU single-thread、CPU multithread、SIMD、GPU の4実装で同じ処理を比較する。

## 4. Lean 4 で検証すべきこと

| 性質                             | 具体的な定理                              | 優先度 |
| ------------------------------ | ----------------------------------- | --- |
| Conservation of value          | Ledger の通貨別総額は transfer で保存される      | P0  |
| Balanced postings              | すべての transaction の debit/credit が一致 | P0  |
| Non-negative available balance | 制約付き口座で利用可能残高が負にならない                | P0  |
| Idempotency                    | 同じ key / payload の再実行で効果が増えない       | P0  |
| Valid state transitions        | 不正な payment state 遷移を拒否する           | P0  |
| Tenant isolation               | 異なる tenant の口座を不正に操作できない            | P0  |
| Batch serializability          | 承認された batch の結果が有効な逐次実行に対応する        | P0  |
| Refund safety                  | 返金累計が返金可能額を超えない                     | P1  |
| Policy soundness               | 許可されない支払いを承認しない                     | P1  |
| CPU/GPU equivalence            | GPU の計算結果が CPU の仕様と一致               | P1  |

### Ledger の形式モデル

S = (A, B, T, I)：A accounts、B balances、T 確定済み transactions、I idempotency key の記録。

apply : S × Transfer → Except Error S

Valid(S) ∧ apply(S, t) = ok(S') ⟹ Valid(S') を Lean 4 で証明する。applyBatch についても同様の定理を証明する。

最初のバージョン:
- GPU：候補バッチの検査・計算
- CPU：権限・残高・状態を最終確認
- PostgreSQL：serializable transaction または適切な row locking による atomic commit

### h5i-app に還元する機能

1. Transaction-aware verified kernel integration
2. DB transaction / retry boundary の明確化
3. Concurrent state machine testing
4. Rust ↔ Lean 4 の仕様対応と proof assumptions のレポート
5. CPU/GPU differential testing
6. Crash injection、recovery testing、migration regression

h5i のセキュリティ文書は、GPU execution は differential tests で確認しており formal proof はしていないと明記している。GPU 正当性の保証を強めるには追加の検証設計が必要。

## 5. AWS hosting の構成

Clients → Route 53 + ALB + AWS WAF → Private VPC:
- ECS Fargate: h5i-app Payment API (CPU) — Auth、Ledger、Payments、Webhooks
- RDS PostgreSQL: Ledger / Transactions / Idempotency
- SQS: Async jobs / DLQ
- AWS Batch + EC2 GPU: Reconciliation、Bulk validation、Aggregation（on-demand）
- S3: Settlement files、Batch input/output、Audit exports
- External PSPs: Stripe test mode → additional PSPs

| コンポーネント        | 推奨                         | 理由                            |
| -------------- | -------------------------- | ----------------------------- |
| Payment API    | ECS Fargate                | 常時稼働、GPU 不要                   |
| PostgreSQL     | RDS                        | Transaction、durability        |
| GPU worker     | AWS Batch + EC2            | ジョブ単位で起動・終了                   |
| Object storage | S3                         | 大量データの入出力                     |
| Queue          | SQS + DLQ                  | 非同期処理と retry                  |
| Secrets        | Secrets Manager + KMS      | PSP API keys / DB credentials |
| Monitoring     | CloudWatch + OpenTelemetry | Metrics、tracing、alerts        |
| Deployment     | Terraform / OpenTofu       | 再現可能な IaC                     |
| CI/CD          | GitHub Actions + ECR       | Build、proof checks、deployment |

GPU インスタンス: g4dn.xlarge (T4, 16 GiB) から開始。g5.xlarge / g6 系は比較対象。ECS G6f fractional GPU も選択肢。GPU vCPU quota に注意。開発はローカル GPU、AWS 側は CPU fallback を残す。

## 6. 正確性を守るための本番設計

| 領域                  | 必要な対策                                                        |
| ------------------- | ------------------------------------------------------------ |
| DB failure          | Atomic transaction、recovery、backup/restore testing           |
| Concurrent payments | Lock ordering、contention tests、serializability               |
| External PSP        | Timeout 後の状態照会、重複送信防止                                        |
| Webhooks            | 署名検証、重複イベント排除、順序逆転への対応                                       |
| Idempotency         | 同一 key と payload の対応、期限後の再利用ポリシー                             |
| Security            | Tenant isolation、least privilege、secret rotation             |
| GPU                 | CPU fallback、differential testing、corrupted output rejection |
| Observability       | Ledger imbalance alerts、unmatched settlement、queue lag       |
| Deployment          | Canary rollout、schema migration rollback strategy            |
| Compliance          | PCI DSS scope、監査・保存・規制上の責任分界                                 |

RDS は本番では Multi-AZ と復旧テスト。AWS が PCI DSS 対象でもアプリ自体の準拠を意味しない。カード情報を扱わない設計にする。

## 7. Repository structure と開発順序

```
payment-platform/
├── Cargo.toml
├── crates/
│   ├── payment-api/         # h5i-app HTTP server
│   ├── payment-domain/      # Pure Rust state machines
│   ├── payment-ledger/      # Double-entry ledger
│   ├── payment-connectors/  # Stripe, Mock PSP
│   ├── payment-batch/       # Batch processing API
│   ├── payment-gpu/         # GPU kernels & adapters
│   ├── payment-reconcile/   # Matching / exceptions
│   └── payment-storage/     # PostgreSQL adapter
├── proofs/
│   ├── Ledger/
│   ├── Payments/
│   ├── Batch/
│   └── Policies/
├── infra/
│   ├── terraform/
│   └── docker/
├── benchmarks/
│   ├── ledger/
│   ├── reconciliation/
│   └── gpu/
└── tests/
    ├── integration/
    ├── concurrency/
    ├── fault-injection/
    └── differential/
```

`payment-domain` と `payment-ledger` はネットワーク、DB、GPU への依存を持たない純粋な Rust 実装にする（h5i-app と Aeneas / Lean 4 の境界を明確にするため）。

### 開発マイルストーン

- M1 — Verified Ledger: Account / transfer / postings、transaction API、idempotency、PostgreSQL integration、Lean 4 の基本不変条件。完了条件：並行 transfer、再実行、DB failure のテストが通る。
- M2 — Payment Orchestration: Mock PSP、Stripe test mode、payment lifecycle、refund、webhooks、retry recovery。完了条件：test mode で決済・取消・返金・照合まで一通り動く。
- M3 — GPU Batch Processing: Reconciliation、batch validation、aggregation の CPU/GPU 実装と benchmark。完了条件：GPU/CPU の出力一致と、GPU が有利になるデータサイズの特定。
- M4 — AWS Deployment: ECS、RDS、SQS、AWS Batch、S3、CI/CD、monitoring、backup/restore。完了条件：AWS 上で安全な test-mode の end-to-end 動作を継続できる。
- M5 — Production Hardening: Security review、failure recovery、runbooks、proof assumptions、performance regression、compliance assessment。完了条件：実資金を扱う前の独立したリスクレビューに進める状態。

## 8. 今の段階で固定したい設計判断

| 設計上の問い         | 推奨する決定                                              |
| -------------- | --------------------------------------------------- |
| 何を作る？          | Open-source payment orchestration + verified ledger |
| Agent 専用？      | いいえ。Agent payment は後から拡張                            |
| 言語             | Rust + Lean 4                                       |
| Web framework  | h5i-app                                             |
| 最初の PSP        | Mock + Stripe                                       |
| Ledger storage | PostgreSQL                                          |
| Consistency    | Atomic DB transactions                              |
| GPU の第一対象      | Reconciliation                                      |
| GPU の第二対象      | Batch policy validation                             |
| GPU commit     | 初期版では採用しない                                          |
| AWS API        | ECS Fargate                                         |
| AWS GPU        | AWS Batch + EC2                                     |
| 最初の運用          | Test mode / synthetic funds                         |
| OSS で参考にするもの   | Hyperswitch、TigerBeetle、Formance、Blnk               |

決済処理全体の TPS を GPU で改善することを最初の成功指標にしない。プロジェクト全体の目標は「Lean 4 で financial correctness を検証でき、CPU/GPU の batch engine を備えた、実際にデプロイ可能な Rust payment infrastructure」。M1 と M2 で正常な決済基盤を作り、M3 で GPU acceleration を性能面で証明する。
