# 第36章 Wire ProtocolとClient/Server

第35章までのRDBMSは、複数のトランザクションを実スレッドから並行実行できます。
`Arc<SharedDatabase>`を複数のスレッドで共有し、`tests/concurrent_threads.rs`は口座間送金を模したトランザクションを本物のOSスレッドから同時に走らせ、ロストアップデートもデッドロックの放置も起きないことを確認済みです。

けれど、その`SharedDatabase`に触れられるのは誰でしょうか。

`Arc::clone`できるのは、同じプロセスの中でスレッドを立てたコード自身だけです。
別のプロセスで動く`psql`のようなクライアント、別のマシンで動くアプリケーションサーバーは、この`Arc`を1つも共有していません。
第1章でこの教材が最後に掲げた到達点、「TCP経由でクライアントから接続できるClient/Server型のRDBMS」からすれば、この章までのRDBMSはまだ実行ファイルの中に閉じ込められたライブラリのままです。
この章では、そこにTCPサーバーとCLIクライアントを足し、`SharedDatabase`をプロセスの外から呼べるようにします。

## TCPはバイトストリームである

サーバーとクライアントの間にTCP接続を1本張れば、それで通信は終わりだと考えたくなります。
実際には、TCPが提供するのは「送った順序通りに届く、信頼できるバイトストリーム」だけです。
`stream.write_all(b"SELECT 1")`を2回呼んでも、受信側の`read`が1回で16バイト全部を受け取れる保証はありません。
5バイトだけ届いて残りは次の`read`で届くこともあれば、逆に2回分の書き込みが1回の`read`で連結されて届くこともあります。
TCPのソケットバッファは、アプリケーションが「ここが1メッセージの区切り」と思っている位置を、何も知らずに素通りしていきます。

したがって、この章が最初に確立しなければならない不変条件は次の1文です。

```text
メッセージの境界は、送信側が長さを明示しない限り、受信側は復元できない。
```

区切り文字(`\n`など)で済ませる案も考えられますが、SQL文自体が改行を含みうるうえ、結果セットの行データはバイナリを含む可能性があります。
区切り文字がペイロードの中に偶然出現しないことを送信側が保証し続けるのは脆く、`SELECT`が返す`TEXT`列の値を1つ変えるだけでその保証が壊れます。
この章では、メッセージの先頭に「これから何バイト読めばこのメッセージが終わるか」を書く、長さ付きフレーミングを採用します。

## 独自プロトコルを選ぶ理由

フレーミングの問題を解決する通信プロトコルは、この教材が自作しなくてもすでに存在します。
PostgreSQLのWire Protocolはその代表で、多くのクライアントライブラリがすでに対応しています。
それでもこの教材が自作の長さ付きフレームを選ぶ理由は、第1章の技術選定表にすでに書きました。

| 領域 | 採用する方式 | 扱わない代替方式 |
| --- | --- | --- |
| 通信 | 独自の単純なフレームプロトコル | PostgreSQL Wire Protocol |

PostgreSQL Wire Protocolは、認証方式(`SCRAM-SHA-256`等)、メッセージ種別の多さ(`Parse`、`Bind`、`Describe`、`Execute`、`Sync`だけで1往復のクエリが5種類に分かれる)、拡張プロトコルと簡易プロトコルの使い分けなど、実運用のクライアントとの互換性を保つための仕様がそれ自体で分厚い領域を持っています。
この章が学びたいのは、フレーミング、リクエストとレスポンスの対応、エラー伝達という通信の基礎であり、既存プロトコルの互換実装に時間を使うと、その基礎より先に仕様の再現作業が主役になってしまいます。
第1章の非目標がすでに認証やTLSによる暗号化を教材の範囲外としているのも、同じ理由の裏返しです。
既存プロトコルへの互換は、この教材が完成したあとの発展編に譲ります。

## フレームのレイアウト

この章で新しく作成する`src/protocol.rs`がフレームを定義します。
リクエストとレスポンスのどちらも同じ9バイトのヘッダを持ちます。

```text
+----------+----------------+------------------+------------------+
| tag(1)   | request_id(4)  | payload_len(4)   | payload(payload_len)|
+----------+----------------+------------------+------------------+
```

- **tag**: リクエストではメッセージ種別、レスポンスではステータスを表します。
- **request_id**: 呼び出し側が採番する識別子です。レスポンスは対応するリクエストの`request_id`をそのまま書き戻します。この章のサーバーは1本の接続の中でリクエストを1件ずつ順に処理するため、1接続だけを見れば対応関係は到着順から追えます。`request_id`を独立したフィールドとして持たせているのは、複数のリクエストを応答を待たずに送りつけるパイプライン化や、非同期クライアントを将来追加したときに、どの応答がどのリクエストのものかをクライアント側で突き合わせるためです。
- **payload_len**: ペイロードのバイト数(`u32`)です。

`src/protocol.rs`の`ProtocolError`は、フレームの読み書きに失敗したときにこれらの関数が返すエラー型です。

```rust
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("I/Oエラー: {0}")]
    Io(#[from] io::Error),
    #[error("フレームが大きすぎます: {len}バイト(上限{max}バイト)")]
    FrameTooLarge {
        len: u32,
        max: u32,
    },
    #[error("未知のメッセージ種別です: 0x{0:02x}")]
    UnknownMessageType(u8),
    #[error("未知のステータスです: 0x{0:02x}")]
    UnknownStatus(u8),
    #[error("ペイロードの形式が不正です: {0}")]
    MalformedPayload(String),
    #[error("UTF-8として不正なバイト列です")]
    InvalidUtf8,
    #[error(transparent)]
    Tuple(#[from] DbError),
}
```

整数フィールドはすべて、ページファイル(第11章)やタプルのエンコード(`crate::tuple_codec`)と同じ、手書きのリトルエンディアン(`to_le_bytes`/`from_le_bytes`)で、`src/protocol.rs`に次のように書きます。

```rust
fn write_raw_frame(writer: &mut impl Write, tag: u8, request_id: u32, payload: &[u8]) -> Result<(), ProtocolError> {
    let payload_len = match u32::try_from(payload.len()) {
        Ok(len) if len <= MAX_FRAME_PAYLOAD_LEN => len,
        Ok(len) => return Err(ProtocolError::FrameTooLarge { len, max: MAX_FRAME_PAYLOAD_LEN }),
        Err(_) => return Err(ProtocolError::FrameTooLarge { len: u32::MAX, max: MAX_FRAME_PAYLOAD_LEN }),
    };
    let mut header = [0u8; FRAME_HEADER_LEN];
    header[0] = tag;
    header[1..5].copy_from_slice(&request_id.to_le_bytes());
    header[5..9].copy_from_slice(&payload_len.to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod protocol;
```

書き出し側もこの時点で`MAX_FRAME_PAYLOAD_LEN`を検査している点が、後述「フレーム長の上限とDoS防止」の読み取り側の検査と対称になっています。

リクエストのペイロードは、実行するSQL文をそのままUTF-8バイト列にしたものです。
`tag`には`MSG_QUERY`(`0x01`)という値だけを定義します。
この章のクライアントが送るメッセージはSQLの実行要求1種類しかなく、複数の種別を用意する理由がまだありません。

レスポンスの`tag`は3種類です。

| `tag` | 意味 | ペイロード |
| --- | --- | --- |
| `STATUS_OK_ROWS`(`0x00`) | `SELECT`、`EXPLAIN`の結果 | 列メタデータ+行の並び |
| `STATUS_OK_COMMAND`(`0x01`) | DDL、DMLの完了 | コマンドタグ文字列(`"CREATE TABLE"`、`"INSERT 2"`等) |
| `STATUS_ERROR`(`0x02`) | 実行時エラー | エラーメッセージ文字列 |

### フレーム長の上限とDoS防止

`payload_len`は送信側の自己申告に過ぎません。
これを無条件に信用して`vec![0u8; payload_len as usize]`を確保すると、悪意のある、あるいは単に壊れたクライアントが`payload_len`に`u32::MAX`(4GiB弱)を書き込むだけで、受信側に4GiB近いメモリを確保させられます。
この章では、`src/protocol.rs`の`read_raw_frame`が、`payload_len`が16MiB(`MAX_FRAME_PAYLOAD_LEN`)を超えるフレームを、ペイロードを1バイトも読まずに拒否します。

`src/protocol.rs`の`RawFrame`は、フレームヘッダを読んだ直後の、まだ意味を解釈していない生のフレームです。

```rust
struct RawFrame {
    tag: u8,
    request_id: u32,
    payload: Vec<u8>,
}
```

```rust
fn read_raw_frame(reader: &mut impl Read) -> Result<RawFrame, ProtocolError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    reader.read_exact(&mut header)?;
    let tag = header[0];
    let request_id = u32::from_le_bytes(header[1..5].try_into().unwrap());
    let payload_len = u32::from_le_bytes(header[5..9].try_into().unwrap());
    if payload_len > MAX_FRAME_PAYLOAD_LEN {
        return Err(ProtocolError::FrameTooLarge { len: payload_len, max: MAX_FRAME_PAYLOAD_LEN });
    }
    let mut payload = vec![0u8; payload_len as usize];
    reader.read_exact(&mut payload)?;
    Ok(RawFrame { tag, request_id, payload })
}
```

16MiBという値そのものに理論的な根拠はありません。
このSQLサブセットの文や、この章までの結果セットが実用上収まる範囲に、余裕を持たせて選んだ目安です。
章が進んで大きな結果セットのストリーミングを扱うようになれば、この定数は見直しの対象になります。

`src/protocol.rs`の`Response`は、クライアントへ返す応答を表す型です。

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Rows {
        schema: Schema,
        rows: Vec<Tuple>,
    },
    Command(String),
    Error(String),
}
```

読み取り側だけでなく、書き出し側の`write_raw_frame`も同じ`MAX_FRAME_PAYLOAD_LEN`を検査します。
書き出し側の検査が無いと、サーバーが`payload_len`の上限を超える`Response`を実際に書き出せてしまい、そのバイト列を同じ上限を守る公式クライアント自身が読めないという非対称が生まれます。
`SELECT`の結果セットが大きくなるほど`Response::Rows`のペイロードは大きくなるため、この非対称は現実に起こりえます。

`src/protocol.rs`の`Response::write`は、`Response::Rows`のペイロードが上限を超えていたら、`write_raw_frame`が`FrameTooLarge`を返すより前に検査し、行を1件も書き出さずに小さな`STATUS_ERROR`応答へ差し替えます。

```rust
Response::Rows { schema, rows } => {
    let payload = encode_rows_payload(schema, rows)?;
    if payload.len() > MAX_FRAME_PAYLOAD_LEN as usize {
        let message = format!(
            "結果が大きすぎて返せません({}バイト、上限{}バイト)。LIMITで件数を絞るか、絞り込む条件を追加してください。",
            payload.len(),
            MAX_FRAME_PAYLOAD_LEN
        );
        return write_raw_frame(stream, STATUS_ERROR, request_id, message.as_bytes());
    }
    write_raw_frame(stream, STATUS_OK_ROWS, request_id, &payload)
}
```

`src/protocol.rs`の`Request`は、クライアントが送るリクエストを表す型です。

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub request_id: u32,
    pub sql: String,
}
```

結果件数の上限やページング、ストリーミングによる分割送信は、この章の範囲を超える変更になるため導入しません。
上限を超えた`SELECT`はエラーとして扱う、この章での最小限の対処にとどめ、大きな結果セットを分割して返す設計は章末の演習課題に譲ります。
`Request::write`(SQLを送る側)も同じ`write_raw_frame`を経由するため、上限を超えるSQL文字列を送ろうとすれば、1バイトも送信せずに`FrameTooLarge`を返します。

### 不正なフレームの扱い

フレームの`tag`が未知の値だったり、ペイロードが期待する形式に従っていなかったりする場合も拒否します。
`tag`が読めている以上、フレームの境界自体は正しく認識できているようにも見えます。
しかし、この章のサーバー(`src/server.rs`)は、フレーミング自体の異常を検出したら、そのフレームだけを捨てて次のフレームを待つのではなく、接続そのものを切断します。

理由は、フレーミングの異常を検出した時点で、その先のバイト列がどこから始まるのか、送信側と受信側の認識がもう食い違っている可能性を排除できないからです。
未知の`tag`は、クライアントが将来のバージョンの新しいメッセージ種別を送ってきただけで、ペイロードの長さ自体は正しいのかもしれません。
あるいは、そもそも`tag`の位置がずれていて、本来は別のフィールドだったバイトを`tag`として読んでしまっているのかもしれません。
この2つを見分ける手段を、受信側は持っていません。
安全側に倒して接続ごと切断する単純な実装にとどめ、フレームごとに続行するかどうかを判断する込み入った回復ロジックは、実運用の要件が増える章に譲ります。

## 結果セットのシリアライズ

`STATUS_OK_ROWS`のペイロードは、列メタデータと行の並びをこの順に書きます。

```text
column_count(4)
column_count回繰り返し:
  name_len(4) name(UTF-8, name_len) data_type(1) nullable(1)
row_count(4)
row_count回繰り返し:
  row_len(4) row(crate::tuple_codec::encode_tupleの出力, row_len)
```

行本体のエンコードには、第12章から存在する`crate::tuple_codec::encode_tuple`/`decode_tuple`をそのまま再利用しています。
`tuple_codec`は「1行の値の並びを、`Schema`が分かっている前提でバイト列に変換する」というモジュールです。
元々の用途はSlotted Pageへ書き込む1レコード分のペイロードでしたが、「`Schema`を1回どこかで確定させ、以後の値の並びはそのSchemaを前提に読み書きする」という関係は、ページに書き込む場合でもフレームに乗せる場合でも変わりません。
違うのは、Slotted Pageの場合はテーブル定義(カタログ)が`Schema`の情報源になるのに対し、この章ではフレーム自身の先頭にSchema(列名、型、NULL許容)を書き込む点です。
`tuple_codec`自身は列名や型といったSchemaの情報を一切知らないため、Schemaのエンコードは、この章で新しく`src/protocol.rs`に書いています。

```rust
fn encode_rows_payload(schema: &Schema, rows: &[Tuple]) -> Result<Vec<u8>, ProtocolError> {
    let mut out = Vec::new();
    let column_count = u32::try_from(schema.len()).expect("この章の列数はu32に収まる");
    out.extend_from_slice(&column_count.to_le_bytes());
    for column in schema.columns() {
        let name_bytes = column.name.as_bytes();
        let name_len = u32::try_from(name_bytes.len()).expect("列名の長さはu32に収まる");
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.push(encode_data_type(column.data_type));
        out.push(u8::from(column.nullable));
    }

    let row_count = u32::try_from(rows.len()).expect("この章の行数はu32に収まる");
    out.extend_from_slice(&row_count.to_le_bytes());
    for tuple in rows {
        let encoded = crate::tuple_codec::encode_tuple(schema, tuple);
        let row_len = u32::try_from(encoded.len()).expect("1行のバイト数はu32に収まる");
        out.extend_from_slice(&row_len.to_le_bytes());
        out.extend_from_slice(&encoded);
    }
    Ok(out)
}
```

`crate::database::QueryResult`をそのままシリアライズする案もありえましたが、採りませんでした。
`QueryResult`はコマンドタグを`Option<String>`として内部的に持ち回り、`Display`実装の中でだけそれを文字列へ変換しています(`schema`、`rows`という2つの公開アクセサはあっても、コマンドタグ自体を取り出す公開APIはありません)。
そこでこのモジュールが接する境界は、`Display`実装と`schema()`/`rows()`という、すでに公開されているAPIの上に置きます。
列を持たない結果(`schema().is_empty()`、DDL、DMLの完了)は`Display`実装(`to_string()`)がそのままコマンドタグ文字列を返すため`Response::Command`に、列を持つ結果(`SELECT`、`EXPLAIN`)は`Response::Rows`に、`src/protocol.rs`の次の`from_db_result`が詰め替えます。

```rust
pub fn from_db_result(result: crate::error::DbResult<crate::database::QueryResult>) -> Response {
    match result {
        Ok(query_result) => {
            if query_result.schema().is_empty() {
                Response::Command(query_result.to_string())
            } else {
                Response::Rows { schema: query_result.schema().clone(), rows: query_result.rows().to_vec() }
            }
        }
        Err(err) => Response::Error(err.to_string()),
    }
}
```

## Blocking I/Oのサーバー: 接続ごとに1本のスレッド

第1章の非目標がすでに書いている通り、この教材はTokioなどによる非同期I/Oを、コアエンジン完成後の別編に回しています。
先に非同期を選ぶと、ストレージエンジンの設計自体が非同期ランタイムの制約を前提にしたものになり、同期実装との差分が見えなくなるからでした。
この章のサーバーは、その方針通りBlocking I/Oで実装します。

並行モデルは最も単純な形、接続を受け付けるたびに`std::thread::spawn`でスレッドを1本立てる方式を採ります。

`src/server.rs`の`Server`は、TCP接続を受け付けるサーバー本体です。

```rust
pub struct Server {
    listener: TcpListener,
    shared: Arc<SharedDatabase>,
}
```

続けて、次の`Server::run`を実装します。

```rust
pub fn run(self) -> std::io::Result<()> {
    for stream in self.listener.incoming() {
        let stream = stream?;
        let shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            handle_connection(stream, &shared);
        });
    }
    Ok(())
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod server;
```

接続数がそのままスレッド数になるため、大量の接続を長時間張られる用途には向きません。
接続をワーカースレッドの集合へ束ねるスレッドプールは第38章(実行制御)の仕事です。
この章では、「複数のクライアントプロセスが同時にこのサーバーへつながる」という、第35章までのRDBMSには無かった性質だけを、最短の実装で確立します。

`SharedDatabase`はすでに`Mutex`1本でスレッド間排他を行っています(第35章)。
複数の接続スレッドが同時にSQLを送ってきても、それぞれの`handle_connection`は`SharedDatabase`の内側で直列化されるため、この章のサーバー自身がロックを追加で管理する必要はありません。

## 接続単位のトランザクション状態

第30章以来、`Database`が通常のSQL経路で同時に持てるトランザクションは高々1本でした(`self.tx: Option<TransactionContext>`)。
複数の接続が同時に`BEGIN`すれば、この制約はもう成り立ちません。
接続ごとに独立したトランザクション状態を持てる仕組みが要ります。

この制約を解消する方法は2つ考えられました。
1つは、第30章の決定的インターリーブテストハーネスが使っている`harness_contexts`(`TransactionId`ごとの`TransactionContext`の対応表)の仕組みを、接続の識別にそのまま転用することです。
もう1つは、`Database`の内部を直接いじらず、`SharedDatabase`がすでに公開している`begin_tx`/`execute_in_tx`/`commit_tx`/`rollback_tx`という`TxHandle`ベースのAPIを、接続ごとに1個ずつ使うことです。
この2つは対立する選択肢ではありません。
`SharedDatabase`の`TxHandle`API自体が`harness_contexts`の上に実装されているため、後者を選べば前者の仕組みをそのまま使うことになります。
この章では、`Database`側を一切変更せずに済む後者を採用しました。

問題は、`src/database.rs`が持つ`SharedDatabase::execute_in_tx`が、`Database::execute`の先頭で行っている`BEGIN`、`COMMIT`、`ROLLBACK`の分岐を経由しないことです。

```rust
pub fn execute_in_tx(&mut self, handle: &TxHandle, sql: &str) -> DbResult<QueryResult> {
    let ctx = self
        .harness_contexts
        .remove(&handle.0)
        .expect("TxHandleはすでにcommit_tx・rollback_tx済み、または他のDatabaseのものです");
    // ...
    let statement = crate::parser::parse_statement(sql);
    let result = match statement {
        Ok(statement) => self.execute_bound_statement(statement, sql),
        Err(err) => Err(err),
    };
    // ...
}
```

`execute_in_tx`はパースした文をいきなり`execute_bound_statement`へ渡しており、`Statement::Begin`、`Commit`、`Rollback`、`Checkpoint`をそこで特別扱いする分岐がありません。
これらの文をバインドした`BoundStatement`は、`execute_bound_statement`の`match`の中で`unreachable!()`に到達します。
`BEGIN`をSQL文字列としてそのまま`execute_in_tx`へ渡すと、この章のサーバースレッドはpanicします。

そこで`src/server.rs`の`Session`は、クライアントから届いたSQLを一度パースし、`BEGIN`、`COMMIT`、`ROLLBACK`だけを`SharedDatabase`の`TxHandle`API(`begin_tx_with_isolation`、`commit_tx`、`rollback_tx`)へ翻訳してから、残りの文だけを`execute_in_tx`へ渡します。

```rust
fn execute_inner(&mut self, sql: &str) -> DbResult<QueryResult> {
    let statement = crate::parser::parse_statement(sql)?;
    match statement {
        Statement::Begin(begin) => {
            if self.tx.is_some() {
                return Err(DbError::TransactionAlreadyActive);
            }
            let level = begin
                .isolation_level
                .unwrap_or(crate::ast::IsolationLevel::RepeatableRead);
            self.tx = Some(self.shared.begin_tx_with_isolation(level));
            Ok(QueryResult::command("BEGIN"))
        }
        Statement::Commit(_) => {
            let handle = self.tx.take().ok_or(DbError::NoActiveTransaction)?;
            self.shared.commit_tx(handle)?;
            Ok(QueryResult::command("COMMIT"))
        }
        Statement::Rollback(_) => {
            let handle = self.tx.take().ok_or(DbError::NoActiveTransaction)?;
            self.shared.rollback_tx(handle)?;
            Ok(QueryResult::command("ROLLBACK"))
        }
        Statement::Checkpoint(_) => Err(DbError::NotImplemented(
            "CHECKPOINTはWire Protocol経由のセッションでは未対応です(第36章の範囲外)".to_string(),
        )),
        _ => match &self.tx {
            Some(handle) => self.shared.execute_in_tx(handle, sql),
            None => self.execute_autocommit(sql),
        },
    }
}
```

この翻訳は、`Database::execute`の先頭にある分岐とほぼ同じ形をしています。
接続ごとのトランザクション状態を`Database`自身にではなく接続の側(`Session`)に持たせる設計を選んだ以上、`BEGIN`等をどちらの状態(`Database::tx`か、この`Session::tx`か)へ反映するかの判断も、状態を持つ側で行う必要があります。
この重複を「セッションの状態と責務を1箇所にまとめる」形で解消するのは、第37章がSessionを正式に導入する仕事です。
この章の`Session`は、接続ごとに独立したトランザクション状態を持てるという最小限の性質だけを満たす前身にとどめます。

明示的な`BEGIN`が無いまま届いた文(Autocommit)は、`src/server.rs`の`execute_autocommit`が、1文だけのために`begin_tx`でトランザクションを開き、成功すれば`commit_tx`、失敗すれば`rollback_tx`します。

```rust
fn execute_autocommit(&self, sql: &str) -> DbResult<QueryResult> {
    let handle = self.shared.begin_tx();
    match self.shared.execute_in_tx(&handle, sql) {
        Ok(result) => {
            self.shared.commit_tx(handle)?;
            Ok(result)
        }
        Err(err) => {
            // `rollback_tx`自体の失敗より、元の実行エラーの方が呼び出し
            // 元にとって重要な情報なので、`rollback_tx`のエラーは握り
            // つぶす。
            let _ = self.shared.rollback_tx(handle);
            Err(err)
        }
    }
}
```

これは`Database::execute`のAutocommit経路(`self.tx`が`None`のとき、`lock_owner`が文ごとの`TransactionId`を割り当ててロックを文の終わりで手放す経路、第31章)と、結果として得られる直列化の単位が同じです。

`CHECKPOINT`は、`SharedDatabase`が`TxHandle`API越しに公開していません。
`Database::execute_checkpoint`はトランザクション境界の外側の操作であり、`harness_contexts`のAPIには乗らない設計だからです(第34章)。
この章のサーバー経由では未対応とし、クライアントへは`DbError::NotImplemented`をエラー応答として返します。

### 切断時のトランザクション後始末

接続が(正常な`\q`ではなく)途中で切れた場合、そのセッションが持っていた未コミットのトランザクションをどうするかを決めておかないと、そのトランザクションが獲得したロック(第31章)を他の接続が永久に待たされます。
この章では`src/server.rs`の`Session`の`Drop`実装で、保持中の`TxHandle`があれば無条件に`rollback_tx`します。

```rust
impl Drop for Session<'_> {
    /// 接続が切れた時点で未コミットのトランザクションが残っていれば
    /// ROLLBACKする(モジュール冒頭「切断時のトランザクション後始末」を参照)。
    fn drop(&mut self) {
        if let Some(handle) = self.tx.take() {
            let _ = self.shared.rollback_tx(handle);
        }
    }
}
```

`Session`の`Drop`は、正常な切断、異常な切断、パニックのどの経路でも通ります。
`src/server.rs`の`handle_connection`のループは、フレームの読み書きに失敗したら`break`するだけで、切断の理由ごとに個別の後始末を書いてはいません。

```rust
fn handle_connection(mut stream: TcpStream, shared: &SharedDatabase) {
    let mut session = Session::new(shared);
    loop {
        let request = match Request::read(&mut stream) {
            Ok(request) => request,
            Err(ProtocolError::Io(err)) if err.kind() == ErrorKind::UnexpectedEof => {
                // クライアントが次のフレームを送る前にソケットを閉じた
                // (正常な切断)。
                break;
            }
            Err(_) => break,
        };

        let response = session.execute(&request.sql);
        if response.write(&mut stream, request.request_id).is_err() {
            // 応答を書き出せなかった(クライアントが読む前に切断した等)。
            // これ以上このストリームへ書いても仕方が無いので接続を終える。
            break;
        }
    }
    // `session`がここでdropされ、保持中のトランザクションがあればROLLBACK
    // される(`Session`の`Drop`実装、モジュール冒頭を参照)。
}
```

後始末を`Drop`という1箇所に集約できるのは、どの`break`もこの関数を抜ける前に`session`を必ず破棄するという、Rustの所有権の規律に乗っているからです。
接続が切れた理由ごとに`match`で後始末を分岐させる設計であれば、分岐を1つ書き忘れるだけでロックの解放漏れが生まれます。

## CLIクライアントと共通の実行経路

第1章は、「Embedded APIをそのままサーバープロセスの内部で呼び出す構造にする」と宣言していました。
この章のCLIクライアント(`src/bin/minidb_client.rs`)とサーバー(`src/server.rs`)が実際にどの経路を共有しているかを確認します。

REPL(`src/main.rs`)は、標準入力から読んだ1行を`Database::execute`へ直接渡していました。

```rust
match db.execute(input) {
    Ok(result) => println!("{result}"),
    Err(e) => println!("エラー: {e}"),
}
```

この章で新しく作成する`src/bin/minidb_client.rs`のCLIクライアントは、`Database::execute`の代わりに`Request`をフレームへ詰めて送り、返ってきた`Response`を表示します。
バイナリ名を`minidb-client`にするため、`Cargo.toml`にも`name = "minidb-client"`、`path = "src/bin/minidb_client.rs"`という`[[bin]]`エントリを追加します。

`send`が返すエラーとして次の`ClientError`を定義します。

```rust
#[derive(Debug)]
enum ClientError {
    Protocol(minidb::ProtocolError),
    RequestIdMismatch { sent: u32, received: u32 },
}
```

`?`によるエラー変換と`{e}`による表示のために、`Display`と`From<minidb::ProtocolError>`を次のように実装します。

```rust
impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Protocol(err) => write!(f, "{err}"),
            ClientError::RequestIdMismatch { sent, received } => {
                write!(f, "応答のrequest_idが一致しません: 送信は{sent}でしたが受信は{received}でした")
            }
        }
    }
}

impl From<minidb::ProtocolError> for ClientError {
    fn from(err: minidb::ProtocolError) -> Self {
        ClientError::Protocol(err)
    }
}
```

```rust
fn send(stream: &mut TcpStream, request_id: u32, sql: &str) -> Result<Response, ClientError> {
    let request = Request { request_id, sql: sql.to_string() };
    request.write(stream)?;
    let (received_id, response) = Response::read(stream)?;
    if received_id != request_id {
        return Err(ClientError::RequestIdMismatch { sent: request_id, received: received_id });
    }
    Ok(response)
}
```

受信した`request_id`は捨てず、送信した`request_id`と一致することを検証しています。
この章のサーバーは1本の接続の中でリクエストを1件ずつ順に処理するため、`request_id`が食い違うことは本来起こらないはずです。
それでも検証せずに応答をそのまま表示すると、フレームの境界がどこかでずれていた場合(モジュール冒頭「不正なフレームの扱い」を参照)に、別のリクエストの結果を気付かず表示しかねません。
`ClientError`は`minidb::ProtocolError`(フレームそのものが読み書きできなかったエラー)にこの不一致を追加した、クライアント側だけの型です。
不一致を検出したら、個別のクエリのエラー(`Response::Error`)とは違い通信そのものの異常として扱い、接続を終了します。

対話UI(標準入力を1行ずつ読み、`\q`で終了し、結果を表示してから次のプロンプトを出す)は2つのバイナリでまったく同じ形をしています。
違うのはSQLの実行先だけです。
REPLは同じプロセス内の`Database`を直接呼び、CLIクライアントはTCP接続の向こうにいる`Server`の`Session`にSQLを実行してもらいます。
その`Session`自身も、最終的には同じ`SharedDatabase::execute_in_tx`/`begin_tx`/`commit_tx`/`rollback_tx`を呼んでいます。
つまりCLIクライアントとサーバーの`Session`は、SQLの実行そのものについては同じ`SharedDatabase`の同じAPIを共有しており、REPLと分岐しているのはネットワークの往復という入出力の皮1枚だけです。

サーバーの起動は、既存の`src/main.rs`に`--serve`という起動引数を1つ足すことで実現します。

```rust
match args.next() {
    Some(flag) if flag == "--serve" => run_server(args),
    first_arg => run_repl(first_arg),
}
```

`cargo run -- example.db`(REPL、永続モード)と`cargo run -- --serve 127.0.0.1:5432 example.db`(サーバー、永続モード)は、どちらも同じ`Database::open`でファイルを開いたあと、`Database`を直接使うか`SharedDatabase`で包んでサーバーに渡すかだけが分かれます。

```sh
cargo run --bin minidb-client -- 127.0.0.1:5432
```

このコマンドで、REPLと同じ対話UIのまま、実行だけがネットワーク越しになったクライアントに接続できます。

## テスト

`src/protocol.rs`の単体テストは、フレームのencode/decode往復と、不正なフレームの拒否を確認します。
`Request`、`Response`のどちらも、書き出したバイト列を読み戻すと元の値と一致することを確認する往復テストに加え、次の異常系を確認します。

- `payload_len`が上限を超えるフレームは、ペイロードを読みにいかず`FrameTooLarge`を返す
- 未知のメッセージ種別、未知のステータスは`UnknownMessageType`、`UnknownStatus`を返す
- ペイロードの途中でストリームが終わるフレームはI/Oエラーとして伝わる
- 未知の`DataType`タグを含む行メタデータは`MalformedPayload`を返す

`tests/wire_protocol.rs`は、実際に`TcpListener`をbindしたサーバーへ、`TcpStream`越しにクライアントを模したコードから接続する統合テストです。
ポートは`"127.0.0.1:0"`(OS割り当て)を使い、`Server::local_addr`で実際に割り当てられたポートを取得します。
固定ポートを決め打ちすると、CIやローカルで他のプロセスとポートが衝突してflakyになるためです。

このテストファイルは次を確認します。

- `CREATE TABLE`、`INSERT`、`SELECT`が1本の接続の上で期待通りの結果を返す
- SQLの実行時エラー(存在しないテーブルへの`SELECT`)はエラー応答として返り、接続は切れずに次のクエリを続けられる
- 明示的な`BEGIN`、`COMMIT`で複数の文にまたがるトランザクションが機能する
- `COMMIT`前に接続が切れると、その変更は他の接続から見えない(`Session`の`Drop`によるROLLBACK)
- 複数の接続が同時に互いに素な行を挿入しても、全件がそろう(`SharedDatabase`によるスレッド間の直列化)
- フレーム長が上限を超えるリクエストや未知のメッセージ種別を送ると、接続が切られる

## この章の限界

この章のサーバーは、接続数の分だけOSスレッドを立てます。
接続ごとに1本のスレッドという設計は、この章が確立したい「複数のクライアントプロセスが同時に接続できる」という性質には十分ですが、接続数が増えるとスレッド数がそのまま増え続けます。
ワーカースレッドの集合へ接続を束ねるスレッドプールは第38章の仕事です。

`Session`が持つ状態は、今アクティブな`TxHandle`だけです。
Prepared StatementやParameter Bindingのような、より本格的なセッション状態は第37章が追加します。
`BEGIN`、`COMMIT`、`ROLLBACK`を`Session`側でSQLとしてパースし直す設計も、この章の限界の1つです。
`Database::execute`の先頭にある同じ分岐と重複しており、第37章でセッションの状態と責務を1箇所にまとめるときに解消されるべき重複として残しています。

サーバーの停止は、プロセスの終了(Ctrl-C等)に頼る単純なものです。
実行中の接続を待ってから止める、新規接続の受付だけを先に止めるといったGraceful Shutdownは第38章の範囲です。

`CHECKPOINT`はこの章のサーバー経由では実行できません。
`SharedDatabase`が`TxHandle`API越しに`Database::execute_checkpoint`を公開していないためで、クライアントへは`DbError::NotImplemented`が返ります。

## 演習問題

### 必須課題

1. `Session::execute_autocommit`は、`execute_in_tx`が成功したあと`commit_tx`を呼び、その`commit_tx`自体が失敗したらそのエラーをそのまま呼び出し元へ返します。この場合、`begin_tx`で確保した`TxHandle`が獲得していたロックはどうなるか、`SharedDatabase::commit_tx`のコードを読んで説明してください。ロックが解放されないまま残る経路があるとしたら、それはどのような操作(2回目の`commit_tx`、`rollback_tx`)で解消できるか考えてください。
2. この章のサーバーは、フレーミング自体の異常(未知のメッセージ種別、フレーム長の上限超過)を検出したら接続を切断します。この設計を、「異常を検出したフレームだけを捨てて、次のフレームの先頭を探しにいく」設計に変えたとして、なぜその探索が一般には不可能か(合成攻撃、たまたま次のフレームの先頭に見えるバイト列)を、具体的なバイト列の例を1つ作って説明してください。
3. `tests/wire_protocol.rs`の`an_uncommitted_transaction_is_rolled_back_when_the_connection_drops`は、切断のあとに`thread::sleep`を挟んでからロールバックの完了を確認しています。この`sleep`を取り除くと、テストがどのような理由でflakyになりうるか説明したうえで、`sleep`に頼らずに済む同期の方法(たとえば別の接続からの応答待ちをそのまま同期点として使う)を実装してください。

### 発展課題

1. `minidb_client`は、送信した`request_id`と受信した`request_id`が一致することを検証するだけで、1本の接続の中で複数のリクエストを応答を待たずに送りつけるパイプライン化までは実装していません。`Request`/`Response`のレイアウトを変えずにパイプライン化を実装し、`request_id`を使って(順不同で返ってきうる)対応するレスポンスを正しく突き合わせるクライアントを書いてください。サーバー側が1リクエストずつ順に処理する設計のままで、クライアント側のパイプライン化にどれだけ意味があるか(レイテンシの隠蔽、スループットへの効果)も考察してください。
2. `encode_rows_payload`は、結果セット全体を1つの`Vec<u8>`に組み立ててから1フレームとして送ります。行数が多いクエリでは、この方式はメモリ上に結果セット全体のコピーを2つ(`QueryResult::rows`と、そこからエンコードした`Vec<u8>`)持つことになります。行を一定件数ごとに分割し、複数のレスポンスフレームに分けて送るストリーミング方式を設計し、実装してください。フレームの最後を示す仕組み(最終フレームであることを示すフラグ、または行数0のフレームを終端とする、等)から設計する必要があります。
3. この章の`Server::run`は、接続を受け付けるたびに無条件で`std::thread::spawn`します。接続数に上限を設け、上限に達している間は新規接続をすぐには`accept`しない(あるいは`accept`はするが即座にエラー応答を返して切断する)設計に変更し、大量の同時接続に対してサーバープロセスのスレッド数が無限に増え続けないことをテストで確認してください。
