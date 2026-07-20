//! 独自の長さ付きフレームプロトコル(第36章)。
//!
//! TCPは1本のバイトストリームを提供するだけで、「ここまでが1件のメッセージ」
//! という区切りを何も知らない。送信側が`db.execute(sql)`を2回呼んで2つの
//! フレームを書き出しても、受信側のソケットバッファの上では両者が連結された
//! 1本のバイト列になりうるし、逆に1回の`read`が1メッセージ分に満たないバイト数
//! しか返さないこともある。したがってこのモジュールが最初に守る不変条件は、
//! **メッセージの境界は、送信側が長さを明示しない限り復元できない**という一点
//! である。この不変条件を満たすため、全てのメッセージの先頭に「これから何
//! バイト読めばこのメッセージが終わるか」を書く、長さ付きフレーミングを採用する。
//!
//! PostgreSQL Wire Protocolのような既存プロトコルを採らない理由は、第1章の
//! 技術選定表に記した通りである。認証方式やメッセージ種別の多さは、通信の
//! 基本(フレーミング、リクエスト/レスポンスの対応、エラー伝達)を学ぶという
//! この章の目的からすると回り道になる。この章では、その基本だけを持つ最小限の
//! フレームを自作する。
//!
//! # フレームの共通レイアウト
//!
//! リクエスト・レスポンスのどちらも、次の9バイトのヘッダに続けてペイロードを
//! 置くという同じ形をしている(ヘッダの1バイト目の意味だけがリクエストと
//! レスポンスで異なる)。
//!
//! ```text
//! +----------+----------------+------------------+------------------+
//! | tag(1)   | request_id(4)  | payload_len(4)   | payload(payload_len)|
//! +----------+----------------+------------------+------------------+
//! ```
//!
//! - **tag**: リクエストでは[`MSG_QUERY`]のような**メッセージ種別**、
//!   レスポンスでは[`STATUS_OK_ROWS`]のような**ステータス**を表す。
//! - **request_id**: 呼び出し側が採番する識別子。レスポンスは、対応する
//!   リクエストの`request_id`をそのまま書き戻す。この章のサーバーは1本の
//!   接続の中でリクエストを1件ずつ順に処理する(ブロッキングI/O、本文
//!   参照)ため、1接続だけを見れば返ってくる順序で対応関係は追える。
//!   `request_id`は、複数のリクエストを応答を待たずに送りつける
//!   パイプライン化や非同期クライアントを将来追加したときに、どの応答が
//!   どのリクエストのものかをクライアント側で突き合わせるための予約
//!   フィールドである。
//! - **payload_len**: ペイロードのバイト数(`u32`)。全ての整数フィールドは
//!   ページファイル(第11章)やタプル(`crate::tuple_codec`)と同じ、手書きの
//!   リトルエンディアン(`to_le_bytes`/`from_le_bytes`)で書く。
//!
//! # フレーム長の上限とDoS防止
//!
//! `payload_len`は送信側の自己申告に過ぎない。これを無条件に信用して
//! `vec![0u8; payload_len as usize]`を確保すると、悪意または壊れたクライアントが
//! `payload_len`に`u32::MAX`(4GiB弱)を書き込むだけで、受信側に4GiB近い
//! メモリを確保させられる。この章では[`MAX_FRAME_PAYLOAD_LEN`](16MiB)を
//! 超える`payload_len`を持つフレームを、ペイロードを1バイトも読まずに
//! [`ProtocolError::FrameTooLarge`]として拒否する。16MiBという値は、この
//! サブセットのSQL文やこの章までの結果セットが実用上収まる範囲に、余裕を
//! 持たせて選んだ目安であり、章が進んで大きな結果セットのストリーミングを
//! 扱うようになれば見直しの対象になる。
//!
//! # 不正なフレームの扱い
//!
//! フレームの`tag`が未知の値だったり、ペイロードが期待する形式(UTF-8、
//! 列数と実際の列の並びの整合など)に従っていなかったりする場合も、
//! [`ProtocolError`]を返す。フレーミングそのもの([`read_raw_frame`])が
//! 壊れているわけではない([`ProtocolError::FrameTooLarge`]や
//! ペイロードの途中でストリームが終わる`Io`エラー)場合、そのフレームより
//! 後のバイト列がどこから始まるのか、送信側と受信側の認識がもうずれている
//! 可能性がある。したがってこの章のサーバー(`crate::server`)は、
//! フレーミング自体の異常を検出したら、そのフレームだけを捨てて処理を
//! 続けるのではなく、接続そのものを切断する。ペイロードの中身の異常
//! (未知の`tag`、壊れたUTF-8)は、フレームの境界自体は正しく読めているため、
//! 個別のエラーとして扱えるが、この章では区別せず両方とも接続を切断する
//! 単純な実装にとどめる(個別のエラー応答を返してから接続を維持するかどうかの
//! 判断は、実運用の要件が増える章に譲る)。

use std::io::{self, Read, Write};

use crate::error::DbError;
use crate::types::{Column, DataType, Schema, Tuple};

/// 1フレームのペイロードとして許すバイト数の上限(DoS防止、モジュール冒頭を参照)。
pub const MAX_FRAME_PAYLOAD_LEN: u32 = 16 * 1024 * 1024;

/// リクエストの`tag`: SQLを1文実行する(この章で唯一のメッセージ種別)。
pub const MSG_QUERY: u8 = 0x01;

/// レスポンスの`tag`: 列メタデータと行の並びを返す(`SELECT`・`EXPLAIN`)。
pub const STATUS_OK_ROWS: u8 = 0x00;
/// レスポンスの`tag`: コマンドタグ(`"CREATE TABLE"`・`"INSERT 2"`等)を返す
/// (DDL・DML)。
pub const STATUS_OK_COMMAND: u8 = 0x01;
/// レスポンスの`tag`: エラーメッセージを返す。
pub const STATUS_ERROR: u8 = 0x02;

/// このモジュールが返すエラー。
///
/// [`DbError`](クエリの実行結果として起きるエラー)とは別の型にしている。
/// `DbError`はSQLの意味論に関するエラー(構文エラー、テーブルが無い等)であり、
/// フレームとしては正しく読めた1件のクエリの**結果**として、[`Response::Error`]
/// に載せてクライアントへ返す。一方この`ProtocolError`は、フレームそのものが
/// 読めない・解釈できないエラーであり、後続のバイト列を1文のクエリの応答として
/// 続けて処理してよいという前提自体が崩れている(モジュール冒頭「不正なフレームの
/// 扱い」を参照)。
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// ソケットの読み書きに失敗した(相手が切断した場合を含む)。
    #[error("I/Oエラー: {0}")]
    Io(#[from] io::Error),
    /// `payload_len`が[`MAX_FRAME_PAYLOAD_LEN`]を超えていた。
    #[error("フレームが大きすぎます: {len}バイト(上限{max}バイト)")]
    FrameTooLarge {
        /// 送信側が申告した`payload_len`。
        len: u32,
        /// 許容する上限([`MAX_FRAME_PAYLOAD_LEN`])。
        max: u32,
    },
    /// リクエストの`tag`が[`MSG_QUERY`]以外だった。
    #[error("未知のメッセージ種別です: 0x{0:02x}")]
    UnknownMessageType(u8),
    /// レスポンスの`tag`が[`STATUS_OK_ROWS`]・[`STATUS_OK_COMMAND`]・
    /// [`STATUS_ERROR`]のいずれでもなかった。
    #[error("未知のステータスです: 0x{0:02x}")]
    UnknownStatus(u8),
    /// ペイロードのバイト数が、宣言された構造(列数、文字列長など)から
    /// 期待される長さに足りなかった。
    #[error("ペイロードの形式が不正です: {0}")]
    MalformedPayload(String),
    /// 文字列として読むべきバイト列が妥当なUTF-8ではなかった。
    #[error("UTF-8として不正なバイト列です")]
    InvalidUtf8,
    /// 行のペイロードを`crate::tuple_codec::decode_tuple`へ渡したところ、
    /// タプルとして復元できなかった。
    #[error(transparent)]
    Tuple(#[from] DbError),
}

/// クエリを1文実行するリクエスト。
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// 呼び出し側が採番する識別子(モジュール冒頭を参照)。
    pub request_id: u32,
    /// 実行するSQL文(UTF-8)。
    pub sql: String,
}

impl Request {
    /// `stream`へこのリクエストを1フレームとして書き出す。
    pub fn write(&self, stream: &mut impl Write) -> Result<(), ProtocolError> {
        write_raw_frame(stream, MSG_QUERY, self.request_id, self.sql.as_bytes())
    }

    /// `stream`から1フレームを読み、[`Request`]として解釈する。
    pub fn read(stream: &mut impl Read) -> Result<Self, ProtocolError> {
        let frame = read_raw_frame(stream)?;
        if frame.tag != MSG_QUERY {
            return Err(ProtocolError::UnknownMessageType(frame.tag));
        }
        let sql = String::from_utf8(frame.payload).map_err(|_| ProtocolError::InvalidUtf8)?;
        Ok(Request { request_id: frame.request_id, sql })
    }
}

/// クエリの実行結果。
///
/// `crate::database::QueryResult`をそのままシリアライズするのではなく、
/// この3種類に単純化している。`QueryResult`はコマンドタグを`Option<String>`と
/// `Display`で内部的に持ち回っているが、フィールドの詳細は非公開であり、
/// このモジュールが直接分解する手段が無い(意図的にそうなっている、
/// `crate::database`のドキュメント参照)。そこで境界を`Display`実装と
/// `schema()`/`rows()`という公開APIの上に置く。列を持たない結果
/// (`schema().is_empty()`、DDL・DMLの完了)は`to_string()`がそのまま
/// コマンドタグ文字列を返すため、[`Response::Command`]に詰め替えられる。
/// 列を持つ結果(`SELECT`・`EXPLAIN`)は[`Response::Rows`]に詰め替える。
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// 列メタデータと行の並び。
    Rows {
        /// 列名・型・NULL許容。
        schema: Schema,
        /// 行の並び(`schema`に従う)。
        rows: Vec<Tuple>,
    },
    /// コマンドタグ(`"CREATE TABLE"`・`"INSERT 2"`等)。
    Command(String),
    /// エラーメッセージ(`DbError`の`Display`表示)。
    Error(String),
}

impl Response {
    /// `crate::database::Database::execute`系のAPIが返す
    /// `DbResult<QueryResult>`を[`Response`]へ変換する。
    ///
    /// `Ok`の場合、`QueryResult::schema()`が空かどうかで[`Response::Rows`]と
    /// [`Response::Command`]を振り分ける(空ならDDL・DMLの完了、`SELECT`・
    /// `EXPLAIN`の結果が空の列構成を持つことは無い)。`Err`の場合は
    /// `DbError`の`Display`表示をそのまま[`Response::Error`]に載せる。
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

    /// `stream`へこのレスポンスを1フレームとして書き出す。
    ///
    /// [`Response::Rows`]のペイロードが[`MAX_FRAME_PAYLOAD_LEN`]を超える場合、
    /// そのまま[`write_raw_frame`]へ渡すと`FrameTooLarge`エラーになり(書き出し側も
    /// 上限を検査する、モジュール冒頭「フレーム長の上限とDoS防止」を参照)、
    /// サーバー自身がクエリの実行結果を1つも返せず接続が切れてしまう。
    /// この章はページングやストリーミングまでは実装せず、上限を超えた場合は
    /// 結果行を1件も送らず、`STATUS_ERROR`の小さなエラー応答へ差し替える
    /// 最小限の対処にとどめる(章末の演習課題を参照)。
    pub fn write(&self, stream: &mut impl Write, request_id: u32) -> Result<(), ProtocolError> {
        match self {
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
            Response::Command(tag) => write_raw_frame(stream, STATUS_OK_COMMAND, request_id, tag.as_bytes()),
            Response::Error(message) => write_raw_frame(stream, STATUS_ERROR, request_id, message.as_bytes()),
        }
    }

    /// `stream`から1フレームを読み、[`Response`]として解釈する。戻り値は
    /// `(request_id, Response)`(呼び出し側が対応するリクエストと突き合わせる
    /// ための`request_id`、モジュール冒頭を参照)。
    pub fn read(stream: &mut impl Read) -> Result<(u32, Response), ProtocolError> {
        let frame = read_raw_frame(stream)?;
        let response = match frame.tag {
            STATUS_OK_ROWS => decode_rows_payload(&frame.payload)?,
            STATUS_OK_COMMAND => {
                let tag = String::from_utf8(frame.payload).map_err(|_| ProtocolError::InvalidUtf8)?;
                Response::Command(tag)
            }
            STATUS_ERROR => {
                let message = String::from_utf8(frame.payload).map_err(|_| ProtocolError::InvalidUtf8)?;
                Response::Error(message)
            }
            other => return Err(ProtocolError::UnknownStatus(other)),
        };
        Ok((frame.request_id, response))
    }
}

impl std::fmt::Display for Response {
    /// CLIクライアント(`src/bin/minidb_client.rs`)の表示形式。
    /// `crate::database::QueryResult`の`Display`実装(REPL、`src/main.rs`)と
    /// 同じ見た目(ヘッダ行、区切り線、`(n rows)`)を再現する。`QueryResult`
    /// 自身をクライアント側では持てない(ネットワーク越しに送れるのは
    /// [`Response`]へ詰め替えたあとのデータだけ)ため、同じ形式をここで
    /// 独立に実装している。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Response::Command(tag) => write!(f, "{tag}"),
            Response::Error(message) => write!(f, "エラー: {message}"),
            Response::Rows { schema, rows } => {
                let header = schema.columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(" | ");
                writeln!(f, "{header}")?;
                writeln!(f, "{}", "-".repeat(header.chars().count().max(1)))?;
                for tuple in rows {
                    let row = tuple.values().iter().map(crate::types::Value::to_string).collect::<Vec<_>>().join(" | ");
                    writeln!(f, "{row}")?;
                }
                let row_word = if rows.len() == 1 { "row" } else { "rows" };
                write!(f, "({} {row_word})", rows.len())
            }
        }
    }
}

/// フレームヘッダを読んだ直後の、まだ意味を解釈していない生のフレーム。
struct RawFrame {
    tag: u8,
    request_id: u32,
    payload: Vec<u8>,
}

const FRAME_HEADER_LEN: usize = 1 + 4 + 4;

/// `tag`・`request_id`・`payload`を、モジュール冒頭のレイアウトに従って
/// `writer`へ書き出す。
///
/// [`read_raw_frame`]が受信側で[`MAX_FRAME_PAYLOAD_LEN`]を検査するのと対称に、
/// この関数も送信側で同じ上限を検査する。この検査が無いと、サーバーが
/// 書き出せた大きなフレームを、同じ上限を守る公式クライアント自身が
/// 読めないという非対称が生まれる(モジュール冒頭「フレーム長の上限とDoS
/// 防止」を参照)。
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

/// `reader`から1フレーム分(ヘッダ+ペイロード)を読む。`payload_len`が
/// [`MAX_FRAME_PAYLOAD_LEN`]を超えていたら、ペイロードを1バイトも読まずに
/// 拒否する(モジュール冒頭「フレーム長の上限とDoS防止」を参照)。
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

/// `DataType`をペイロード上の1バイトのタグへ変換する。
fn encode_data_type(data_type: DataType) -> u8 {
    match data_type {
        DataType::Boolean => 0,
        DataType::BigInt => 1,
        DataType::Text => 2,
    }
}

/// ペイロード上の1バイトのタグから`DataType`を復元する。
fn decode_data_type(tag: u8) -> Result<DataType, ProtocolError> {
    match tag {
        0 => Ok(DataType::Boolean),
        1 => Ok(DataType::BigInt),
        2 => Ok(DataType::Text),
        other => Err(ProtocolError::MalformedPayload(format!("未知のDataTypeタグです: {other}"))),
    }
}

/// [`STATUS_OK_ROWS`]のペイロードを組み立てる。
///
/// # レイアウト
///
/// ```text
/// column_count(4)
/// column_count回繰り返し:
///   name_len(4) name(UTF-8, name_len) data_type(1) nullable(1)
/// row_count(4)
/// row_count回繰り返し:
///   row_len(4) row(crate::tuple_codec::encode_tupleの出力, row_len)
/// ```
///
/// 行本体のエンコードは`crate::tuple_codec::encode_tuple`をそのまま再利用する。
/// `tuple_codec`は「1行の値の並び」をschemaが分かっている前提でエンコードする
/// モジュールであり(第12章、Slotted Pageへ書き込む1レコード分のペイロードが
/// 元々の用途)、ここでも「先頭にschemaを1回だけ書き、以後の各行はそのschemaを
/// 前提にした値だけを書く」という同じ関係が成り立つ。`tuple_codec`自身は
/// schema(列名・型)を一切知らないため、schemaのエンコード(このペイロードの
/// 前半)はこのモジュールで新しく書く必要がある。
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

/// 列メタデータ1件が最低限占めるバイト数(`name_len(4)` + `data_type(1)` +
/// `nullable(1)`、列名が空文字列の場合の下限)。[`decode_rows_payload`]が
/// `column_count`を確保する前に、残りバイト数からこの下限を使って
/// 「この`column_count`は絶対にありえない」ことを検査する。
const MIN_COLUMN_ENTRY_LEN: usize = 4 + 1 + 1;

/// 行1件が最低限占めるバイト数(`row_len(4)`、行本体が0バイトの場合の下限)。
/// [`decode_rows_payload`]が`row_count`を確保する前の検査に使う
/// ([`MIN_COLUMN_ENTRY_LEN`]と同じ理由)。
const MIN_ROW_ENTRY_LEN: usize = 4;

/// [`encode_rows_payload`]の逆変換。
///
/// `column_count`・`row_count`は送信側の自己申告であり、[`read_raw_frame`]が
/// 検査する`payload_len`とは独立している(モジュール冒頭「フレーム長の上限と
/// DoS防止」参照)。16MiB以下の短いペイロードに、桁外れに大きな
/// `column_count`・`row_count`だけを書き込めば、`Vec::with_capacity`が
/// その値をそのまま信用して過大な確保を試みてしまう。この関数は、確保の前に
/// 「残りバイト数から見てこの件数はそもそもありえない」ことを検査し、
/// 検査を通っても確保自体は`try_reserve`で行って失敗を`ProtocolError`へ
/// 変換する(OSがメモリ確保に失敗した場合にpanicで落ちないようにするため)。
fn decode_rows_payload(bytes: &[u8]) -> Result<Response, ProtocolError> {
    let mut cursor = 0usize;
    let column_count = read_u32(bytes, &mut cursor)?;
    let remaining = bytes.len() - cursor;
    if (column_count as usize).saturating_mul(MIN_COLUMN_ENTRY_LEN) > remaining {
        return Err(ProtocolError::MalformedPayload(format!(
            "column_countがペイロードの残りバイト数に対して大きすぎます: {column_count}"
        )));
    }
    let mut columns = Vec::new();
    columns
        .try_reserve_exact(column_count as usize)
        .map_err(|_| ProtocolError::MalformedPayload("列メタデータの確保に失敗しました".to_string()))?;
    for _ in 0..column_count {
        let name_len = read_u32(bytes, &mut cursor)?;
        let name = read_utf8(bytes, &mut cursor, name_len as usize)?;
        let data_type = decode_data_type(read_u8(bytes, &mut cursor)?)?;
        let nullable = match read_u8(bytes, &mut cursor)? {
            0 => false,
            1 => true,
            other => {
                return Err(ProtocolError::MalformedPayload(format!(
                    "nullableは0または1である必要があります: {other}"
                )));
            }
        };
        columns.push(Column::new(name, data_type, nullable));
    }
    let schema = Schema::new(columns);

    let row_count = read_u32(bytes, &mut cursor)?;
    let remaining = bytes.len() - cursor;
    if (row_count as usize).saturating_mul(MIN_ROW_ENTRY_LEN) > remaining {
        return Err(ProtocolError::MalformedPayload(format!(
            "row_countがペイロードの残りバイト数に対して大きすぎます: {row_count}"
        )));
    }
    let mut rows = Vec::new();
    rows.try_reserve_exact(row_count as usize)
        .map_err(|_| ProtocolError::MalformedPayload("行の確保に失敗しました".to_string()))?;
    for _ in 0..row_count {
        let row_len = read_u32(bytes, &mut cursor)? as usize;
        let row_end = cursor
            .checked_add(row_len)
            .ok_or_else(|| ProtocolError::MalformedPayload("行の長さがオーバーフローしています".to_string()))?;
        let row_bytes = bytes.get(cursor..row_end).ok_or_else(|| {
            ProtocolError::MalformedPayload("行本体を読む前にペイロードが尽きました".to_string())
        })?;
        cursor = row_end;
        let tuple = crate::tuple_codec::decode_tuple(&schema, row_bytes)?;
        rows.push(tuple);
    }
    if cursor != bytes.len() {
        return Err(ProtocolError::MalformedPayload(
            "ペイロードの末尾に余剰バイトがあります".to_string(),
        ));
    }
    Ok(Response::Rows { schema, rows })
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, ProtocolError> {
    let slice = bytes
        .get(*cursor..*cursor + 4)
        .ok_or_else(|| ProtocolError::MalformedPayload("u32を読む前にペイロードが尽きました".to_string()))?;
    *cursor += 4;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

fn read_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8, ProtocolError> {
    let byte = bytes
        .get(*cursor)
        .copied()
        .ok_or_else(|| ProtocolError::MalformedPayload("u8を読む前にペイロードが尽きました".to_string()))?;
    *cursor += 1;
    Ok(byte)
}

fn read_utf8(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<String, ProtocolError> {
    let slice = bytes
        .get(*cursor..*cursor + len)
        .ok_or_else(|| ProtocolError::MalformedPayload("文字列を読む前にペイロードが尽きました".to_string()))?;
    *cursor += len;
    String::from_utf8(slice.to_vec()).map_err(|_| ProtocolError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use std::io::Cursor;

    fn sample_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::BigInt, false),
            Column::new("name", DataType::Text, true),
        ])
    }

    #[test]
    fn request_round_trips_through_a_byte_stream() {
        let request = Request { request_id: 7, sql: "SELECT 1".to_string() };
        let mut buf = Vec::new();
        request.write(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = Request::read(&mut cursor).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn response_rows_round_trips_with_null_values() {
        let schema = sample_schema();
        let rows = vec![
            Tuple::new(&schema, vec![Value::BigInt(1), Value::Text("Alice".to_string())]).unwrap(),
            Tuple::new(&schema, vec![Value::BigInt(2), Value::Null]).unwrap(),
        ];
        let response = Response::Rows { schema, rows };

        let mut buf = Vec::new();
        response.write(&mut buf, 42).unwrap();
        let mut cursor = Cursor::new(buf);
        let (request_id, decoded) = Response::read(&mut cursor).unwrap();
        assert_eq!(request_id, 42);
        assert_eq!(decoded, response);
    }

    #[test]
    fn response_command_round_trips() {
        let response = Response::Command("INSERT 2".to_string());
        let mut buf = Vec::new();
        response.write(&mut buf, 1).unwrap();
        let mut cursor = Cursor::new(buf);
        let (request_id, decoded) = Response::read(&mut cursor).unwrap();
        assert_eq!(request_id, 1);
        assert_eq!(decoded, response);
    }

    #[test]
    fn response_error_round_trips() {
        let response = Response::Error("テーブルが存在しません: users".to_string());
        let mut buf = Vec::new();
        response.write(&mut buf, 3).unwrap();
        let mut cursor = Cursor::new(buf);
        let (request_id, decoded) = Response::read(&mut cursor).unwrap();
        assert_eq!(request_id, 3);
        assert_eq!(decoded, response);
    }

    #[test]
    fn request_with_empty_schema_and_no_rows_round_trips() {
        let response = Response::Rows { schema: Schema::new(Vec::new()), rows: Vec::new() };
        let mut buf = Vec::new();
        response.write(&mut buf, 0).unwrap();
        let mut cursor = Cursor::new(buf);
        let (_, decoded) = Response::read(&mut cursor).unwrap();
        assert_eq!(decoded, response);
    }

    #[test]
    fn read_rejects_a_frame_whose_payload_exceeds_the_limit() {
        let mut header = Vec::new();
        header.push(MSG_QUERY);
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&(MAX_FRAME_PAYLOAD_LEN + 1).to_le_bytes());
        // ペイロード自体は1バイトも書かない。上限判定はヘッダだけで完結し、
        // ペイロードを読みにいかないことを確認する。
        let mut cursor = Cursor::new(header);
        let err = Request::read(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge { .. }));
    }

    #[test]
    fn read_rejects_an_unknown_request_message_type() {
        let mut buf = Vec::new();
        buf.push(0xFF);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor = Cursor::new(buf);
        let err = Request::read(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::UnknownMessageType(0xFF)));
    }

    #[test]
    fn read_rejects_an_unknown_response_status() {
        let mut buf = Vec::new();
        buf.push(0xFF);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor = Cursor::new(buf);
        let err = Response::read(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::UnknownStatus(0xFF)));
    }

    #[test]
    fn read_rejects_a_frame_truncated_before_the_full_payload_arrives() {
        let mut buf = Vec::new();
        buf.push(MSG_QUERY);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&10u32.to_le_bytes()); // 10バイト読む、と申告
        buf.extend_from_slice(b"short"); // 実際には5バイトしか続かない
        let mut cursor = Cursor::new(buf);
        let err = Request::read(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::Io(_)));
    }

    #[test]
    fn decode_rows_payload_rejects_an_unknown_data_type_tag() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes()); // column_count = 1
        let name = b"c";
        payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
        payload.extend_from_slice(name);
        payload.push(0xFF); // 未知のDataTypeタグ
        payload.push(0);
        let err = decode_rows_payload(&payload).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }

    // ---- 第6部レビュー対応: 書き出し側の上限検査、decode_rows_payloadの防御 ----

    #[test]
    fn request_write_accepts_a_payload_exactly_at_the_limit() {
        let sql = "x".repeat(MAX_FRAME_PAYLOAD_LEN as usize);
        let request = Request { request_id: 1, sql };
        let mut buf = Vec::new();
        request.write(&mut buf).unwrap();
    }

    #[test]
    fn request_write_rejects_a_payload_one_byte_over_the_limit() {
        let sql = "x".repeat(MAX_FRAME_PAYLOAD_LEN as usize + 1);
        let request = Request { request_id: 1, sql };
        let mut buf = Vec::new();
        let err = request.write(&mut buf).unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge { .. }), "{err:?}");
        // 送信前にエラーになり、1バイトも書き出さない。
        assert!(buf.is_empty());
    }

    #[test]
    fn response_write_converts_an_oversized_rows_payload_into_a_small_error_response() {
        // 1行のTEXTだけで上限を超える巨大な結果セットを組み立てる。
        let schema = Schema::new(vec![Column::new("s", DataType::Text, false)]);
        let big_value = Value::Text("x".repeat(MAX_FRAME_PAYLOAD_LEN as usize));
        let rows = vec![Tuple::new(&schema, vec![big_value]).unwrap()];
        let response = Response::Rows { schema, rows };

        let mut buf = Vec::new();
        response.write(&mut buf, 7).unwrap();

        // 書き出したフレーム自体は小さく、`Response::read`でSTATUS_ERRORとして
        // 読み戻せる(公式クライアントが読める大きさに収まっている)。
        assert!((buf.len() as u32) < MAX_FRAME_PAYLOAD_LEN);
        let mut cursor = Cursor::new(buf);
        let (request_id, decoded) = Response::read(&mut cursor).unwrap();
        assert_eq!(request_id, 7);
        assert!(matches!(decoded, Response::Error(_)), "{decoded:?}");
    }

    #[test]
    fn response_write_accepts_a_rows_payload_exactly_at_the_limit() {
        // 空文字列を1行持つペイロードの実測サイズ(schema・row_count・NULL
        // ビットマップ・長さプレフィックス等、全オーバーヘッドを含む)を基準に、
        // TEXTの長さを1バイトずつの差分で逆算する(手計算のオフセットに頼らない)。
        let schema = Schema::new(vec![Column::new("s", DataType::Text, false)]);
        let base_len = encode_rows_payload(&schema, &[Tuple::new(&schema, vec![Value::Text(String::new())]).unwrap()])
            .unwrap()
            .len();
        let text_len = MAX_FRAME_PAYLOAD_LEN as usize - base_len;
        let rows = vec![Tuple::new(&schema, vec![Value::Text("x".repeat(text_len))]).unwrap()];
        let payload_len = encode_rows_payload(&schema, &rows).unwrap().len();
        assert_eq!(payload_len, MAX_FRAME_PAYLOAD_LEN as usize, "テストの前提: ちょうど上限のペイロードを組み立てられている");

        let response = Response::Rows { schema, rows };
        let mut buf = Vec::new();
        response.write(&mut buf, 1).unwrap();
        let mut cursor = Cursor::new(buf);
        let (_, decoded) = Response::read(&mut cursor).unwrap();
        assert!(matches!(decoded, Response::Rows { .. }), "ちょうど上限のペイロードはSTATUS_OK_ROWSのまま読めるはず");
    }

    #[test]
    fn decode_rows_payload_rejects_a_column_count_too_large_for_the_remaining_bytes() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&u32::MAX.to_le_bytes()); // column_count
        // 残りバイトはほぼ無い。
        let err = decode_rows_payload(&payload).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }

    #[test]
    fn decode_rows_payload_rejects_a_row_count_too_large_for_the_remaining_bytes() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // column_count = 0
        payload.extend_from_slice(&u32::MAX.to_le_bytes()); // row_count
        // 残りバイトはほぼ無い。
        let err = decode_rows_payload(&payload).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }

    #[test]
    fn decode_rows_payload_rejects_a_nullable_byte_that_is_neither_0_nor_1() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes()); // column_count = 1
        let name = b"c";
        payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
        payload.extend_from_slice(name);
        payload.push(encode_data_type(DataType::Boolean));
        payload.push(2); // 0でも1でもないnullableバイト
        payload.extend_from_slice(&0u32.to_le_bytes()); // row_count = 0
        let err = decode_rows_payload(&payload).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }

    #[test]
    fn decode_rows_payload_rejects_trailing_bytes_after_the_last_row() {
        let schema = sample_schema();
        let response = Response::Rows { schema: schema.clone(), rows: Vec::new() };
        let mut buf = Vec::new();
        response.write(&mut buf, 0).unwrap();
        // `Response::write`が書いたフレームのペイロード部分の末尾に、
        // 余剰バイトを1つ足す。
        let mut header_and_payload = buf;
        let extra_byte_len = (u32::from_le_bytes(header_and_payload[5..9].try_into().unwrap()) + 1).to_le_bytes();
        header_and_payload[5..9].copy_from_slice(&extra_byte_len);
        header_and_payload.push(0xAB);
        let mut cursor = Cursor::new(header_and_payload);
        let err = Response::read(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }
}
