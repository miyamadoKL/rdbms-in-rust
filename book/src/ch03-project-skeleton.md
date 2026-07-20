# 第3章 Rustプロジェクトの骨格とテスト基盤

前章まででRDBMSの範囲と、SQLがストレージへ到達するまでの経路を確認しました。
まだ何も実装していません。

これから作るストレージ層では、ページを指す番号、テーブルを指す番号、トランザクションを指す番号を扱うことになります。
どれも中身はただの整数であり、`u64`型一つで間に合わせてしまえそうに見えます。
この章では、その素朴な間に合わせが後でどう壊れるかを起点に、プロジェクトの雛形とテストの仕組みを組み立てます。

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

crateは一つだけです。
パーサ、ストレージ、実行エンジンをそれぞれ別crateに分けたくなるかもしれませんが、その分割は今は行いません。
理由は、crate間の境界を先に引くと、後から変更するコストが高くなるからです。
crateを分けるとpub化の範囲やCargo.tomlの依存関係を都度調整する必要があり、実装がまだ固まっていない段階では、その調整自体が頻発します。

ストレージ層の実装がまとまり、パーサやオプティマイザから独立して育てられる状態になった段階で、workspace分割を検討します。
第2部の終わり、あるいはそれ以降が目安です。

`src/main.rs` はまだ何もしません。
そこに、挨拶を1行`println!`で出すだけの内容を書きます。

```rust
fn main() {
    println!("minidb へようこそ。まだ何もできません。");
}
```

ユーザー向けの出力と、後述の診断用ログは責務が異なります。
挨拶は利用者に向けたプログラムの出力なので標準出力(`println!`)へ、ログは標準エラー出力(後述の`log_info!`)へと出し先を分けます。

`src/lib.rs` がクレート本体です。
これから各章のモジュールをここに追加していきます。

## エラー型を先に決める

RDBMSの実装では、I/Oエラー、パースエラー、型エラー、制約違反など、性質の異なるエラーが後から次々に増えます。

`Result<T, String>` のように文字列でエラーを表すと、呼び出し側はエラーの種類を`match`で区別できません。
エラーの原因ごとに異なる処理をしたい場面(たとえば「一意制約違反ならリトライせず、I/Oエラーならリトライする」)で、文字列を解析するはめになります。

そこで、`src/error.rs`を新規作成し、エラー型のenum`DbError`と、その`Result`エイリアス`DbResult`を定義します。

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

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod error;
```

`thiserror` は、`#[error("...")]` からDisplay実装を生成し、`#[from]` から`From`実装を生成するだけのcrateです[^thiserror]。
エラー型の構造自体は素のenumと変わらないため、後から`thiserror`をやめて手書きの実装に戻すことも難しくありません。

[^thiserror]: `anyhow`は呼び出し側でエラーの種類を区別する必要がない、アプリケーションの末端向けの型です。
    ライブラリであるminidbでは、呼び出し側がエラーの種類で分岐できるよう、enumベースの`thiserror`を選びます。

現時点の`DbError`は`Io`と`NotImplemented`の2種類しか持ちません。
`NotImplemented`は、まだ書いていない機能を呼び出したときに使うための、この章専用の仮のバリアントです。
第6〜7章でパーサを実装すれば`Parse`が、第9章でカタログを実装すれば`TableNotFound`や`DuplicateTable`が、といった具合に、章を追うごとにバリアントが増えていきます。

## 生のu64を引き回すと何が起きるか

ストレージ層では、ページを指す識別子、テーブルを指す識別子、トランザクションを指す識別子など、複数の識別子を扱います。

これらをすべて`u64`型で表すとどうなるか、次の関数を例に考えます。

```rust
fn load_page(table_id: u64, page_id: u64) -> DbResult<()> {
    // ...
    Ok(())
}
```

呼び出し側が引数の順番を取り違えて `load_page(page_id, table_id)` と書いても、どちらも`u64`なのでコンパイルは通ります。
実行時にテーブルIDのつもりでページIDを渡してしまい、意図しないページを読み込んでからバグに気づく、という事態が起こり得ます。

この種の間違いは、コンパイラではなく実行結果を見て初めて発覚します。

## Newtypeで型を分ける

`u64`をそのまま使う代わりに、`src/ids.rs`を新規作成し、識別子ごとに専用の型`PageId`、`TableId`、`TransactionId`を定義します。

```rust
/// ディスク上の1ページを指す識別子。
///
/// `TableId` とは型が異なるため、呼び出し側が引数を取り違えてもコンパイルエラーになる。
///
/// ```compile_fail
/// use minidb::{PageId, TableId};
///
/// fn load_page(page_id: PageId) {}
///
/// let table_id = TableId(1);
/// load_page(table_id); // 型が違うためコンパイルエラー
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(pub u64);

/// カタログに登録されたテーブルを指す識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableId(pub u64);

/// トランザクションを指す識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(pub u64);
```

あわせて`src/lib.rs`に次の行を加え、このモジュールを公開します。

```rust
pub mod ids;
```

これは**Newtype**と呼ばれるパターンで、既存の型(ここでは`u64`)を1要素のタプル構造体で包み、別の型として扱えるようにします。

先ほどの`load_page`をNewtypeで書き直すとどうなるか、次の例で確認します。

```rust
fn load_page(table_id: TableId, page_id: PageId) -> DbResult<()> {
    // ...
    Ok(())
}
```

引数を取り違えて `load_page(page_id, table_id)` と書くと、`PageId`が渡るべき位置に`TableId`が渡り、型が一致しないためコンパイルエラーになります。

`derive`した`PartialEq`と`Eq`により、`PageId(1) == PageId(1)`のような比較ができます。
`Hash`は、後の章で`HashMap<PageId, _>`のようにバッファプールのページテーブルを作る際に必要です。
`Clone`と`Copy`は、`u64`同様に値渡しでコピーできるようにするためのものです。

「取り違えるとコンパイルエラーになる」という主張は、`PageId(1) == PageId(1)`のような等価性テストでは検証できません。
そのテストが確かめるのは`derive`した`PartialEq`の動作であって、型の取り違えを防げるかどうかではありません。
取り違えがコンパイルエラーになることそのものを確認するには、実際に`PageId`を要求する関数へ`TableId`を渡すコードを書き、それがコンパイルに失敗することを確かめる必要があります。

Rustの`compile_fail`付きdoctestは、コードブロックがコンパイルに失敗することをテストとして表明できます。
先ほどの`PageId`の定義に付けたdoc commentが、まさにこの`compile_fail`ブロックです。

`cargo test`はこのdoctestを実行し、`load_page(table_id)`が実際にコンパイルエラーになった場合にのみ成功と判定します。
コンパイルが通ってしまえば、doctestは失敗します。

## ログ出力の最小限の仕組み

第2部でストレージ層の実装を始めると、「どのページを読んだか」「どのトランザクションが開始したか」を目視で追いたい場面が出てきます。
その段階になってからログの仕組みを用意するのではなく、診断に使う道具をこの章のうちに先に用意しておきます。

`log`や`tracing`のような専用crateを導入する選択肢もありますが、現時点では依存を増やさず、`eprintln!`を薄くラップしたマクロで済ませます。
クレート全体で使うマクロなので、モジュールではなく`src/lib.rs`に直接置きます。

```rust
/// 簡易ログ出力マクロ(依存追加を避けるため `eprintln!` を薄くラップするだけ)。
///
/// ```
/// minidb::log_info!("starting {}", 1);
/// ```
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        eprintln!("[minidb] {}", format_args!($($arg)*))
    };
}
```

`format!`ではなく`format_args!`を使うのは、`format!`が中間結果として`String`を確保するのに対し、`format_args!`はフォーマット済み文字列を組み立てずに`eprintln!`へそのまま引数を渡せるためです。
ログ出力のたびに使い捨ての`String`を確保する必要がなくなります。

マクロ定義にdoctestを添えたのは、`#[macro_export]`したマクロが未使用のまま放置されるのを防ぐためです。
`cargo test`はこのdoctestを展開してコンパイルと実行を行うため、`log_info!`が実際に呼び出し可能であることを継続的に検証できます。
`src/main.rs`の挨拶出力は利用者向けの通常出力なので`println!`のままとし、`log_info!`は第2部以降、ストレージ層の内部状態を追う場面で使い始めます。

ログレベルの出し分けや出力先の切り替えが必要になった時点で、`tracing`への置き換えを検討します。
今はまだその必要がありません。

## SQL Golden Testの仕組み

ここまでの型はまだSQLを一切実行しません。
それでも、テストの仕組みだけは先に用意しておきます。

理由は、第4章以降にエンジンの実装を進める中で、「SQLを1本実行して、出力が期待通りか確認する」という形式のテストを繰り返し書くことになるからです。
テストの形式を先に固定しておけば、テストケースを追加するだけで済みます。

この形式のテストを、本教材では**SQL Golden Test**と呼びます。
`.sql`ファイルに入力を、`.expected`ファイルに期待する出力を書き、両者をペアとして突き合わせます。
このテストランナーは、新規作成する`tests/golden.rs`に実装します。

```rust
/// `tests/golden/` 以下の `.sql` ファイルを列挙する。
fn collect_sql_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("golden test dir {:?} を開けません: {e}", dir))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("golden test dir {:?} の列挙中にエラー: {e}", dir))
        .into_iter()
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

`collect_sql_files`が`fs::read_dir`の結果を`filter_map(|entry| entry.ok())`のようにエラーを黙って捨てて集めていたなら、権限エラーなどで一部のエントリの列挙に失敗しても、その事実に気づかないままテスト対象のファイルが減ります。
`collect::<Result<Vec<_>, _>>()`で一度`Result`にまとめてから展開すれば、列挙中のエラーは`unwrap_or_else`が捕捉し、`panic!`としてテストの失敗に変換されます。

`run_sql`が肝心のクエリ実行部分ですが、現時点ではエンジンが存在しないため、`tests/golden.rs`には入力をそのまま返すエコーとして仮実装します。

```rust
/// 仮実装: クエリエンジンがまだ無いので、SQLをそのままエコーするだけ。
fn run_sql(sql: &str) -> String {
    sql.to_string()
}
```

これに対応する最初のテストケースが、`tests/golden/001_echo.sql`と`tests/golden/001_echo.expected`です。
両ファイルの中身はどちらも`SELECT 1;`で、エコー実装のもとでは一致します。

このテストは、第5章で`SELECT 1`を実際に実行できるようになった時点で意味を持ち始めます。
`run_sql`の中身を、パーサと実行エンジンの呼び出しへ差し替えていくのは第5章以降の作業です。
ここで確認したいのは、テストの形式自体が先に動くということです。

## Gitタグによる章の区切り

各章の開始時点と完了時点を、`chapter-XX-start`と`chapter-XX-final`という形式のGitタグで記録します。
`XX`は章番号2桁です(この章なら`chapter-03-start`、`chapter-03-final`)。

タグを使う理由は、読者が「第7章の時点のコードを見たい」と思ったときに、章を跨いだ差分やコミット履歴を辿らずに、該当するタグへ直接`git checkout`できるようにするためです。

`chapter-XX-final`と次章の`chapter-(XX+1)-start`は同一のコミットを指します。
章の間でコードは変化しないため、区別する意味がありません。

## 各章の読み方

本教材は、各章を次の7段階の順序で統一します。

1. 前章の実装ではできないことを再現する
2. 必要な理論を説明する
3. 守るべき不変条件を明示する
4. 最小実装を追加する
5. テストを書く
6. 壊して確認する
7. 演習問題を示す

たとえば第14章(Buffer Pool)では、最初に「すべてのタプル参照でディスクI/Oが発生する」状態を計測してからキャッシュを導入します。
第33章(WAL)では、データページだけが書かれてコミットレコードが失われる状況を再現してからログを導入します。

この順序を採る理由は、実装を先に見せてから理論を説明すると、その実装が何を解決しているのかが読者に伝わらないからです。
逆に、先に限界を体感しておけば、後から入る理論や実装が何のためにあるのかが分かった状態で読めます。

この章自体は例外です。
まだ何も実装していない段階なので、再現すべき前章の限界がありません。
土台となる型とテストの仕組みを先に組み立てる、骨格作りに徹する回になっています。

## テストで確認する到達点

`cargo test`は3つのことを確認します。
SQL Golden Testの形式そのものが動くこと(`golden_tests_pass`)、`log_info!`マクロが実際に呼び出し可能であること(`src/lib.rs`のdoctest)、`PageId`と`TableId`を取り違えるコードが実際にコンパイルエラーになること(`src/ids.rs`のdoctest)の3つです。

```console
$ cargo test
running 1 test
test golden_tests_pass ... ok

Doc-tests minidb

running 1 test
test src/lib.rs - log_info (line 14) ... ok

running 1 test
test src/ids.rs - ids::PageId (line 9) - compile fail ... ok
```

この3つが通れば、この章の到達点は完了です。
エラー型、Newtype、ログ出力、Golden Testの仕組みはまだSQLを何も実行しませんが、以降の章がその上に積み上げていく土台として揃いました。
