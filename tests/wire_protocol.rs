//! 実TCPのループバック統合テスト(第36章)。
//!
//! `crate::protocol`のencode/decode往復と不正フレームの拒否は
//! `src/protocol.rs`の単体テストが検証済みである。ここでは、その1段上
//! ―実際に`TcpListener`をbindし、`minidb::Server`を1本の接続ごとに1スレッドで
//! 動かし、`TcpStream`越しにクライアントを模したコードから
//! `CREATE`・`INSERT`・`SELECT`を投げて期待通りの結果が返るか―を検証する。
//!
//! ポートは`"127.0.0.1:0"`(OS割り当て)を使う。固定ポートを決め打ちすると、
//! CIやローカルで他のテスト・プロセスとポートが衝突してflakyになるため。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use minidb::protocol::{MSG_QUERY, ProtocolError, Request, Response};
use minidb::{Database, Server, SharedDatabase};

/// テスト用にサーバーを1本立て、接続先アドレスを返す。
///
/// `Server::run`は呼び出したスレッドを無限にブロックするため、別スレッドで
/// 動かす。このスレッドはテストプロセスが終了するまで生き続けるが、
/// 明示的な停止機構は第38章(Graceful Shutdown)の範囲であり、この章では
/// テストプロセスの終了とともにOSへ後始末を任せる。
fn spawn_server() -> std::net::SocketAddr {
    let shared = Arc::new(SharedDatabase::new(Database::memory()));
    let server = Server::bind("127.0.0.1:0", shared).expect("bindに失敗しました");
    let addr = server.local_addr().expect("local_addrの取得に失敗しました");
    thread::spawn(move || {
        server.run().expect("サーバーの実行に失敗しました");
    });
    addr
}

/// `sql`を1件送り、応答を受け取る。
fn send_sql(stream: &mut TcpStream, request_id: u32, sql: &str) -> Response {
    let request = Request { request_id, sql: sql.to_string() };
    request.write(stream).expect("リクエストの送信に失敗しました");
    let (received_id, response) = Response::read(stream).expect("レスポンスの受信に失敗しました");
    assert_eq!(received_id, request_id, "request_idが一致しません");
    response
}

fn connect(addr: std::net::SocketAddr) -> TcpStream {
    // サーバースレッドがacceptを始めるまでの間、接続が一時的に失敗することが
    // ある(`bind`から`run`が実際に`accept`ループへ入るまでのわずかな遅延)ため、
    // 数回リトライする。
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(addr) {
            return stream;
        }
        thread::sleep(Duration::from_millis(10));
    }
    TcpStream::connect(addr).expect("接続に失敗しました")
}

#[test]
fn create_insert_select_round_trip_over_tcp() {
    let addr = spawn_server();
    let mut stream = connect(addr);

    let response = send_sql(&mut stream, 1, "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)");
    assert_eq!(response, Response::Command("CREATE TABLE".to_string()));

    let response = send_sql(&mut stream, 2, "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')");
    assert_eq!(response, Response::Command("INSERT 2".to_string()));

    let response = send_sql(&mut stream, 3, "SELECT id, name FROM users ORDER BY id");
    match response {
        Response::Rows { schema, rows } => {
            assert_eq!(schema.columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "name"]);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].values()[0], minidb::Value::BigInt(1));
            assert_eq!(rows[0].values()[1], minidb::Value::Text("Alice".to_string()));
            assert_eq!(rows[1].values()[0], minidb::Value::BigInt(2));
        }
        other => panic!("SELECTの結果が行の並びではありません: {other:?}"),
    }
}

#[test]
fn a_sql_error_comes_back_as_an_error_response_without_closing_the_connection() {
    let addr = spawn_server();
    let mut stream = connect(addr);

    let response = send_sql(&mut stream, 1, "SELECT * FROM no_such_table");
    match response {
        Response::Error(message) => assert!(message.contains("no_such_table"), "メッセージ: {message}"),
        other => panic!("エラー応答を期待しましたが{other:?}でした"),
    }

    // エラーのあとも同じ接続でクエリを続けられる(接続そのものは切れていない)。
    let response = send_sql(&mut stream, 2, "CREATE TABLE t (id BIGINT)");
    assert_eq!(response, Response::Command("CREATE TABLE".to_string()));
}

#[test]
fn explicit_begin_commit_makes_a_multi_statement_transaction_visible() {
    let addr = spawn_server();
    let mut stream = connect(addr);

    send_sql(&mut stream, 1, "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT)");
    assert_eq!(send_sql(&mut stream, 2, "BEGIN"), Response::Command("BEGIN".to_string()));
    assert_eq!(send_sql(&mut stream, 3, "INSERT INTO accounts VALUES (1, 100)"), Response::Command("INSERT 1".to_string()));
    assert_eq!(send_sql(&mut stream, 4, "COMMIT"), Response::Command("COMMIT".to_string()));

    let response = send_sql(&mut stream, 5, "SELECT balance FROM accounts WHERE id = 1");
    match response {
        Response::Rows { rows, .. } => assert_eq!(rows[0].values()[0], minidb::Value::BigInt(100)),
        other => panic!("SELECTの結果が行の並びではありません: {other:?}"),
    }
}

/// 接続が`COMMIT`前に切断されたら、そのトランザクションの変更は
/// ROLLBACKされ、他の接続からは見えない(`crate::server`の`Session::drop`)。
#[test]
fn an_uncommitted_transaction_is_rolled_back_when_the_connection_drops() {
    let addr = spawn_server();

    {
        let mut setup = connect(addr);
        let response = send_sql(&mut setup, 1, "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT)");
        assert_eq!(response, Response::Command("CREATE TABLE".to_string()));
    }

    {
        let mut stream = connect(addr);
        assert_eq!(send_sql(&mut stream, 1, "BEGIN"), Response::Command("BEGIN".to_string()));
        assert_eq!(
            send_sql(&mut stream, 2, "INSERT INTO accounts VALUES (1, 100)"),
            Response::Command("INSERT 1".to_string())
        );
        // `COMMIT`を送らずに接続を閉じる。
    }
    // ソケットのクローズがサーバー側へ届くまでの猶予を与える。
    thread::sleep(Duration::from_millis(200));

    let mut checker = connect(addr);
    let response = send_sql(&mut checker, 1, "SELECT * FROM accounts");
    match response {
        Response::Rows { rows, .. } => assert!(rows.is_empty(), "コミットしていない行が見えています: {rows:?}"),
        other => panic!("SELECTの結果が行の並びではありません: {other:?}"),
    }
}

/// 複数の接続が同時に別々の行を挿入しても、全件がそろう
/// (`SharedDatabase`(第35章)がスレッド間の直列化を担う)。
#[test]
fn concurrent_connections_can_insert_disjoint_rows_at_the_same_time() {
    const CONNECTIONS: i64 = 8;
    const PER_CONNECTION: i64 = 20;

    let addr = spawn_server();

    {
        let mut setup = connect(addr);
        send_sql(&mut setup, 1, "CREATE TABLE events (id BIGINT PRIMARY KEY)");
    }

    let handles: Vec<_> = (0..CONNECTIONS)
        .map(|conn_index| {
            thread::spawn(move || {
                let mut stream = connect(addr);
                for i in 0..PER_CONNECTION {
                    let id = conn_index * PER_CONNECTION + i;
                    let sql = format!("INSERT INTO events VALUES ({id})");
                    let response = send_sql(&mut stream, i as u32, &sql);
                    assert_eq!(response, Response::Command("INSERT 1".to_string()));
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("クライアントスレッドがpanicしました");
    }

    let mut checker = connect(addr);
    let response = send_sql(&mut checker, 1, "SELECT COUNT(*) FROM events");
    match response {
        Response::Rows { rows, .. } => {
            assert_eq!(rows[0].values()[0], minidb::Value::BigInt(CONNECTIONS * PER_CONNECTION));
        }
        other => panic!("SELECTの結果が行の並びではありません: {other:?}"),
    }
}

/// `PREPARE`・`EXECUTE`・`DEALLOCATE`はSQL文字列としてそのまま送れる
/// (第37章、Wire Protocolへの追加が不要な理由は`crate::session`モジュール
/// 冒頭「Wire Protocolを拡張しない」を参照)。
#[test]
fn prepare_execute_deallocate_round_trip_over_tcp() {
    let addr = spawn_server();
    let mut stream = connect(addr);

    send_sql(&mut stream, 1, "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)");
    send_sql(&mut stream, 2, "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')");
    assert_eq!(
        send_sql(&mut stream, 3, "PREPARE by_id AS SELECT name FROM users WHERE id = $1"),
        Response::Command("PREPARE".to_string())
    );

    match send_sql(&mut stream, 4, "EXECUTE by_id(2)") {
        Response::Rows { rows, .. } => assert_eq!(rows[0].values()[0], minidb::Value::Text("Bob".to_string())),
        other => panic!("SELECTの結果が行の並びではありません: {other:?}"),
    }

    assert_eq!(send_sql(&mut stream, 5, "DEALLOCATE by_id"), Response::Command("DEALLOCATE".to_string()));
    match send_sql(&mut stream, 6, "EXECUTE by_id(1)") {
        Response::Error(message) => assert!(message.contains("by_id"), "メッセージ: {message}"),
        other => panic!("エラー応答を期待しましたが{other:?}でした"),
    }
}

/// Prepared StatementはSessionごと、つまりTCP接続ごとに独立している
/// (第37章、`crate::session`モジュール冒頭「Prepared StatementはSessionの
/// ものである」を参照)。別の接続で`PREPARE`した名前は見えない。
#[test]
fn prepared_statements_do_not_leak_across_connections() {
    let addr = spawn_server();
    let mut owner = connect(addr);
    send_sql(&mut owner, 1, "CREATE TABLE t (id BIGINT)");
    send_sql(&mut owner, 2, "PREPARE p AS SELECT id FROM t");

    let mut other = connect(addr);
    match send_sql(&mut other, 1, "EXECUTE p()") {
        Response::Error(message) => assert!(message.contains('p'), "メッセージ: {message}"),
        other => panic!("エラー応答を期待しましたが{other:?}でした"),
    }
}

/// フレーム長が上限を超えるリクエストを送ると、サーバーはペイロードを
/// 読みにいかず接続を切る(`crate::protocol`モジュール冒頭を参照)。
#[test]
fn a_frame_exceeding_the_length_limit_causes_the_server_to_close_the_connection() {
    let addr = spawn_server();
    let mut stream = connect(addr);

    let mut header = Vec::new();
    header.push(MSG_QUERY);
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&(minidb::MAX_FRAME_PAYLOAD_LEN + 1).to_le_bytes());
    stream.write_all(&header).expect("ヘッダの送信に失敗しました");
    stream.flush().ok();

    // サーバーは巨大なペイロードを読みにいかず、接続を閉じる。読み取りが
    // EOF(0バイト)またはエラーで終わることを確認する。
    let mut buf = [0u8; 1];
    let result = stream.read(&mut buf);
    match result {
        Ok(0) => {}     // EOF: 接続が閉じられた。
        Err(_) => {}    // 接続がリセットされた場合もあり得る。
        Ok(n) => panic!("接続が閉じられず{n}バイト読めてしまいました"),
    }
}

/// 未知のメッセージ種別を送ると、フレーミングが信用できなくなるため
/// サーバーは接続を切る。
#[test]
fn an_unknown_message_type_causes_the_server_to_close_the_connection() {
    let addr = spawn_server();
    let mut stream = connect(addr);

    let mut buf = Vec::new();
    buf.push(0xFF); // MSG_QUERY(0x01)以外の未知のタグ。
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    stream.write_all(&buf).expect("送信に失敗しました");
    stream.flush().ok();

    let mut reader = &stream;
    let err = Response::read(&mut reader).unwrap_err();
    // クライアント側は、応答フレームが来る前に接続が閉じられたことを
    // I/Oエラー(EOF相当)として観測する。
    assert!(matches!(err, ProtocolError::Io(_)));
}
