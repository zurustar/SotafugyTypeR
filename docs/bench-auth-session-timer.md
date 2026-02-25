# ベンチマークでのDigest認証・セッションタイマー有効化

## はじめに

SIP負荷テストツールのベンチマークスクリプト（`scripts/bench.sh`）において、Digest認証とセッションタイマー（Session-Expires）を有効化しました。認証機能とセッションタイマー処理はUAC/UASに実装済みでしたが、ベンチマークでは認証が無効化されており、INVITEリクエストにSession-Expiresヘッダが付与されていませんでした。本変更により、認証フローとセッションタイマー処理を含む、より実運用に近いベンチマーク結果を取得できるようになりました。

## 概要

変更は以下の3層にわたります。

| 層 | 変更内容 |
|----|---------|
| 設定層 | `Config`構造体に`session_expires`フィールド（u64型、デフォルト300秒）を追加 |
| SIPプロトコル層 | `UacConfig`に`session_expires`（Duration型）を追加し、`build_invite_request`でSession-Expiresヘッダを付与 |
| スクリプト層 | `bench.sh`でユーザファイル生成、Digest認証有効化、Session-Expires設定を実施 |

## 主要な変更点

### Config構造体の拡張

`Config`構造体に`session_expires`フィールドを追加しました。

| フィールド | 型 | デフォルト | バリデーション |
|-----------|-----|----------|--------------|
| session_expires | u64 | 300 | > 0 |

`#[serde(default)]`により、設定ファイルにフィールドが未指定の場合はデフォルト値300（5分）が使用されます。`session_expires`に0が指定された場合はバリデーションエラーを返します。

### UacConfigの拡張

`UacConfig`に`session_expires`フィールド（`Duration`型、デフォルト300秒）を追加しました。`run_load_test`関数内で`Config.session_expires`（u64）を`Duration::from_secs()`で変換してUacConfigに渡します。

### build_invite_requestでのSession-Expiresヘッダ付与

`build_invite_request`関数が生成するINVITEリクエストに、`UacConfig.session_expires`の秒数をSession-Expiresヘッダとして付与するようにしました。UASは200 OKにSession-Expiresを含めて応答し、既存のre-INVITEスケジューリングが動作します。

### bench.shの変更

ベンチマークスクリプトに以下の変更を加えました。

- ベンチマーク実行前にユーザファイル（`/tmp/bench_users.json`）を生成
- プロキシ設定で`auth_enabled=true`、`users_file`にユーザファイルパスを指定
- ロードテスター設定で`auth_enabled=true`、`users_file`指定、`session_expires=300`を設定
- クリーンアップ時にユーザファイルを削除

## 正当性プロパティ

以下のプロパティをproptestで検証済み:

1. Configラウンドトリップ: session_expiresフィールドを含むConfigの直列化→逆直列化が元の値と等価
2. build_invite_requestのSession-Expiresヘッダ付与: 任意のUacConfig・Dialog・UserEntryに対して、生成されるSipRequestにSession-Expiresヘッダが含まれ、値がUacConfig.session_expires.as_secs()の文字列表現と一致
