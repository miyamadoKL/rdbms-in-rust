# 第3章 Rustプロジェクトの骨格とテスト基盤

前章までで、これから作るRDBMSの範囲と、SQLがストレージへ到達するまでの経路を確認しました。本章では、その実装を始めるための土台を作ります。

土台とは、プロジェクトの雛形、エラー型、識別子の型、そしてテストの仕組みです。これらは第4章以降で機能を追加していく際の前提になるため、先に固めておきます。

## 単一crateで始める

`cargo init` で `minidb` という名前のプロジェクトを作ります。

```toml
[package]
name = "minidb"
version = "0.1.0"
edition = "2024"

[dependencies]
thiserror = "2.0.18"
```

crateは一つだけです。パーサ、ストレージ、実行エンジンをそれぞれ別crateに分けたくなるかもしれませんが、その分割は今は行いません。理由は、crate間の境界を先に引くと、後から変更するコストが高くなるからです。crateを分けるとpub化の範囲やCargo.tomlの依存関係を都度調整する必要があり、実装がまだ固まっていない段階では、その調整自体が頻発します。

ストレージ層の実装がまとまり、パーサやオプティマイザから独立して育てられる状態になった段階で、workspace分割を検討します。第2部の終わり、あるいはそれ以降が目安です。

`src/main.rs` はまだ何もしません。

```rust
fn main() {
    println!("minidb へようこそ。まだ何もできません。");
}
```

`src/lib.rs` がクレート本体です。これから各章のモジュールをここに追加していきます。

## エラー型を先に決める

RDBMSの実装では、I/Oエラー、パースエラー、型エラー、制約違反など、性質の異なるエラーが後から次々に増えます。

`Result<T, String>` のように文字列でエラーを表すと、呼び出し側はエラーの種類を`match`で区別できません。エラーの原因ごとに異なる処理をしたい場面(たとえば「一意制約違反ならリトライせず、I/Oエラーならリトライする」)で、文字列を解析するはめになります。

そこで、エラーの種類をenumの列挙子として表します。

```rust
use thiserror::Error;

/// minidb の操作全般で返されるエラー。
#[derive(Debug, Error)]
pub enum DbError {
    /// I/O 由来のエラー(ファイル読み書き等)。
    #[error("I/Oエラー: {0}")]
    Io(#[from] std::io::Error),

    /// まだ実装されていない機能を呼び出したときのエラー。
    #[error("未実装: {0}")]
    NotImplemented(String),
}

/// minidb の操作全般で使う `Result` エイリアス。
pub type DbResult<T> = Result<T, DbError>;
```

`thiserror` は、`#[error("...")]` からDisplay実装を生成し、`#[from]` から`From`実装を生成するだけのcrateです[^thiserror]。エラー型の構造自体は素のenumと変わらないため、後から`thiserror`をやめて手書きの実装に戻すことも難しくありません。

[^thiserror]: `anyhow`は呼び出し側でエラーの種類を区別する必要がない、アプリケーションの末端向けの型です。ライブラリであるminidbでは、呼び出し側がエラーの種類で分岐できるよう、enumベースの`thiserror`を選びます。

現時点の`DbError`は`Io`と`NotImplemented`の2種類しか持ちません。`NotImplemented`は、まだ書いていない機能を呼び出したときに使うための、この章専用の仮のバリアントです。第6〜7章でパーサを実装すれば`Parse`が、第9章でカタログを実装すれば`TableNotFound`や`DuplicateTable`が、といった具合に、章を追うごとにバリアントが増えていきます。

## 生のu64を引き回すと何が起きるか

ストレージ層では、ページを指す識別子、テーブルを指す識別子、トランザクションを指す識別子など、複数の識別子を扱います。

これらをすべて`u64`型で表すとどうなるか、次の関数を例に考えます。

```rust
fn load_page(table_id: u64, page_id: u64) -> DbResult<()> {
    // ...
    Ok(())
}
```

呼び出し側が引数の順番を取り違えて `load_page(page_id, table_id)` と書いても、どちらも`u64`なのでコンパイルは通ります。実行時にテーブルIDのつもりでページIDを渡してしまい、意図しないページを読み込んでからバグに気づく、という事態が起こり得ます。

この種の間違いは、コンパイラではなく実行結果を見て初めて発覚します。

## Newtypeで型を分ける

`u64`をそのまま使う代わりに、識別子ごとに専用の型を定義します。

```rust
/// ディスク上の1ページを指す識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(pub u64);

/// カタログに登録されたテーブルを指す識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableId(pub u64);

/// トランザクションを指す識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(pub u64);
```

これは**Newtype**と呼ばれるパターンで、既存の型(ここでは`u64`)を1要素のタプル構造体で包み、別の型として扱えるようにします。

先ほどの`load_page`をNewtypeで書き直すと、次のようになります。

```rust
fn load_page(table_id: TableId, page_id: PageId) -> DbResult<()> {
    // ...
    Ok(())
}
```

引数を取り違えて `load_page(page_id, table_id)` と書くと、`PageId`が渡るべき位置に`TableId`が渡り、型が一致しないためコンパイルエラーになります。

`derive`した`PartialEq`と`Eq`により、`PageId(1) == PageId(1)`のような比較ができます。`Hash`は、後の章で`HashMap<PageId, _>`のようにバッファプールのページテーブルを作る際に必要です。`Clone`と`Copy`は、`u64`同様に値渡しでコピーできるようにするためのものです。

これらの型が正しく区別されることは、次のテストで確認できます。

```rust
#[test]
fn page_id_equality() {
    assert_eq!(PageId(1), PageId(1));
    assert_ne!(PageId(1), PageId(2));
}
```

## ログ出力の最小限の仕組み

実装を進めると、「どのページを読んだか」「どのトランザクションが開始したか」を目視で追いたい場面が出てきます。

`log`や`tracing`のような専用crateを導入する選択肢もありますが、現時点では依存を増やさず、`eprintln!`を薄くラップしたマクロで済ませます。

```rust
/// 簡易ログ出力マクロ(依存追加を避けるため `eprintln!` を薄くラップするだけ)。
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        eprintln!("[minidb] {}", format!($($arg)*))
    };
}
```

ログレベルの出し分けや出力先の切り替えが必要になった時点で、`tracing`への置き換えを検討します。今はまだその必要がありません。

## SQL Golden Testの仕組み

ここまでの型はまだSQLを一切実行しません。それでも、テストの仕組みだけは先に用意しておきます。

理由は、第4章以降にエンジンの実装を進める中で、「SQLを1本実行して、出力が期待通りか確認する」という形式のテストを繰り返し書くことになるからです。テストの形式を先に固定しておけば、テストケースを追加するだけで済みます。

この形式のテストを、本教材では**SQL Golden Test**と呼びます。`.sql`ファイルに入力を、`.expected`ファイルに期待する出力を書き、両者をペアとして突き合わせます。

```rust
/// `tests/golden/` 以下の `.sql` ファイルを列挙する。
fn collect_sql_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("golden test dir {:?} を開けません: {e}", dir))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    files.sort();
    files
}

#[test]
fn golden_tests_pass() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let sql_files = collect_sql_files(&dir);
    assert!(
        !sql_files.is_empty(),
        "tests/golden/ にサンプルの .sql が見つかりません"
    );

    for sql_path in sql_files {
        let expected_path = sql_path.with_extension("expected");
        assert!(
            expected_path.exists(),
            "{:?} に対応する .expected がありません",
            sql_path
        );

        let sql = fs::read_to_string(&sql_path)
            .unwrap_or_else(|e| panic!("{:?} を読めません: {e}", sql_path));
        let expected = fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("{:?} を読めません: {e}", expected_path));

        let actual = run_sql(&sql);
        assert_eq!(
            actual, expected,
            "golden test 不一致: {:?}",
            sql_path.file_name().unwrap()
        );
    }
}
```

`run_sql`が肝心のクエリ実行部分ですが、現時点ではエンジンが存在しないため、入力をそのまま返すエコーになっています。

```rust
/// 仮実装: クエリエンジンがまだ無いので、SQLをそのままエコーするだけ。
fn run_sql(sql: &str) -> String {
    sql.to_string()
}
```

これに対応する最初のテストケースが、`tests/golden/001_echo.sql`と`tests/golden/001_echo.expected`です。両ファイルの中身はどちらも`SELECT 1;`で、エコー実装のもとでは一致します。

このテストは、第5章で`SELECT 1`を実際に実行できるようになった時点で意味を持ち始めます。`run_sql`の中身を、パーサと実行エンジンの呼び出しへ差し替えていくのは第5章以降の作業です。ここで確認したいのは、テストの形式自体が先に動くということです。

## Gitタグによる章の区切り

各章の開始時点と完了時点を、`chapter-XX-start`と`chapter-XX-final`という形式のGitタグで記録します。`XX`は章番号2桁です(この章なら`chapter-03-start`、`chapter-03-final`)。

タグを使う理由は、読者が「第7章の時点のコードを見たい」と思ったときに、章を跨いだ差分やコミット履歴を辿らずに、該当するタグへ直接`git checkout`できるようにするためです。

`chapter-XX-final`と次章の`chapter-(XX+1)-start`は同一のコミットを指します。章の間でコードは変化しないため、区別する意味がありません。

## 各章の読み方

本教材は、各章を次の7段階の順序で統一します。

1. 前章の実装ではできないことを再現する
2. 必要な理論を説明する
3. 守るべき不変条件を明示する
4. 最小実装を追加する
5. テストを書く
6. 壊して確認する
7. 演習問題を示す

たとえば第14章(Buffer Pool)では、最初に「すべてのタプル参照でディスクI/Oが発生する」状態を計測してからキャッシュを導入します。第33章(WAL)では、データページだけが書かれてコミットレコードが失われる状況を再現してからログを導入します。

この順序を採る理由は、実装を先に見せてから理論を説明すると、その実装が何を解決しているのかが読者に伝わらないからです。逆に、先に限界を体感しておけば、後から入る理論や実装が何のためにあるのかが分かった状態で読めます。

この章自体は例外です。まだ何も実装していない段階なので、再現すべき前章の限界がありません。次章からこの7段階に沿います。

## 章末

ここまでの内容は`cargo test`で確認できます。

```
$ cargo test
running 1 test
test tests::page_id_equality ... ok

running 1 test
test golden_tests_pass ... ok
```

`PageId`の等価性テストとSQL Golden Testの両方が通れば、この章の到達点は完了です。

次章では、まだ手を付けていない`Schema`・`Tuple`・`Value`を型として定義し、リレーショナルモデルをRustの型に落とし込みます。
