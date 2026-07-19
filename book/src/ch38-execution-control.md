# 第38章 実行制御

`SELECT * FROM orders AS a JOIN order_items AS b ON a.id = a.id`のような書き誤りを、第36章のサーバーに投げてみます。

`a.id = a.id`は常に真です。`orders`が1万行、`order_items`が10万行あれば、この`JOIN`は1万×10万=10億通りの組み合わせをすべて評価します。
クライアント側では、応答がいつまでも返ってきません。
サーバー側では、そのクエリを処理しているワーカースレッドがCPUを使い切ったまま、いつ終わるとも知れない計算を続けています。

このクエリを止める手段は、第37章までのサーバーにはありません。
`Ctrl-C`でプロセスを殺すことはできますが、それは「このクエリだけ」ではなく「サーバー全体」を道連れにします。
接続している他のクライアント、進行中の別のトランザクション、まだディスクへ書き戻していない変更、すべてが巻き込まれます。

## 止められないクエリ、閉じないままの接続

第36章のサーバーには、この章で埋めるべき隙間がもう2つあります。

1つは接続数です。
`crate::server::Server::run`は、接続を受け付けるたびに`std::thread::spawn`でスレッドを1本立てていました。
1,000個の接続を同時に張られれば、1,000本のOSスレッドが生まれます。
1本ごとのスタック(既定で数MiB)とOSのスケジューリングコストを踏まえれば、この方式は「同時に何本の接続を処理できるか」という問いへの答えを、サーバー自身ではなく接続してくるクライアントの都合に委ねています。

もう1つは終了時の後始末です。
第37章の`Session`は、接続が切れれば保持中のトランザクションを`Drop`で`ROLLBACK`します。
けれどこれは「1本の接続が切れたとき」の話であり、「サーバープロセスそのものを止めるとき」の話ではありません。
`Ctrl-C`はUnixのデフォルトでは即座にプロセスを終了させます。
その瞬間に未フラッシュのページがバッファプールに残っていれば、そのページの内容は失われます。
WAL(第33章)があるので次回起動時のCrash Recoveryがデータを守ってはくれますが、「終了操作を挟めば安全に片付けられたはずの状態」を、毎回Crash Recoveryに頼るのは筋が違います。

この章は、次の5つを1つずつ足していきます。

- 暴走したクエリを打ち切る**Cancellation**
- 接続数を有限に保つ**Worker Thread Pool**
- 文単位の実行時間に上限を課す**Query Timeout**
- Sort、Hash Joinなどが溜め込むメモリに上限を課す**メモリ使用量の上限**
- 終了時に後始末をしてから止まる**Graceful Shutdown**

## 強制停止ではなく、頼むという設計

冒頭のクエリを、実行中のスレッドの外側から強制的に止められないでしょうか。

Rustの標準ライブラリは、`std::thread::JoinHandle`に「今すぐ止めろ」と伝えるAPIを持ちません。
OSのレベルでは`pthread_cancel`のような強制終了の仕組みが存在する処理系もありますが、この教材はそこへ踏み込みません。
理由は、強制終了が「いつ止まるか」を実行中のコード自身が選べない点にあります。

`storage.insert(table_id, &bytes)`の直後、まだ索引への反映(`storage.index_insert_row`)を終えていない瞬間にスレッドが強制停止すれば、Heapには書き込まれているのに索引には無い行が残ります。
ロックを保持したままのスレッドが停止すれば、そのロックは二度と解放されず、他のすべてのトランザクションが永久に待たされます。
強制停止は「その時点で何をしていたか」を一切考慮しないため、この教材がここまで守ってきた不変条件(索引とHeapの一致、ロックの規律)を、止め方そのものが破壊しかねません。

この章が採るのは**協調的キャンセル**です。
「止めてほしい」という意思表示だけを実行中のコードへ渡し、実行中のコード自身が、ロックも索引も矛盾しない安全な地点まで進んでから、その意思表示を確認して自発的に終了します。
強制力を手放す代わりに、「どこで確認するか」を実行中のコードの側に残せます。

## CancellationTokenと同期ポイント

意思表示そのものは、`crate::cancellation::CancellationToken`という小さな型が運びます。

```rust
#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
    checkpoints: Arc<AtomicUsize>,
}
```

`cancelled`が明示的なキャンセル要求、`deadline`がタイムアウトの締切です(締切は次の節で使います)。
`clone`は`Arc`を複製するだけなので安価で、`clone`した先はすべて同じキャンセル要求を共有します。

意思表示を確認する側は`check`を呼びます。

```rust
pub fn check(&self) -> DbResult<()> {
    self.checkpoints.fetch_add(1, Ordering::Relaxed);
    if let Some(deadline) = self.deadline
        && Instant::now() >= deadline
    {
        return Err(DbError::QueryTimeout);
    }
    if self.cancelled.load(Ordering::Relaxed) {
        return Err(DbError::QueryCancelled);
    }
    Ok(())
}
```

`checkpoints`は本番のコードが読むフィールドではありません。
この章のテストが、「実行中のクエリが実際に同期ポイントを何度も通過した」ことを`sleep`による時間待ちではなく実測で確認するための、呼び出し回数のカウンタです(後述)。

**どこで`check`を呼ぶか**が、この章の設計の核心です。
`next()`を呼ぶたびに`check`すれば理論上もっとも早く気付けますが、`AtomicBool::load`はゼロコストではありません。
この章は、行を1件処理するたびに定数コストが乗っても実害の無い場所にだけ`check`を差し込みます。

1つは`Executor::next()`を駆動する最上位のループです。

```rust
let mut rows = Vec::new();
while let Some(tuple) = executor.next()? {
    ctx.cancel.check()?;
    rows.push(tuple);
}
```

`Filter`や`Projection`のようなstreaming演算子は子から1行引くたびにこのループへ戻ってくるため、`SeqScan`を含む大半の演算子の進捗はここで拾えます。

もう1つは、`Sort`、`Hash Join`のBuild側、`Hash Aggregate`のように、子を`None`まで読み切ってから初めて1行返す**blocking演算子**です。
これらは最上位のループへ戻ってくる前に、内部で何万行も読み進めることがあります。
`SortExec::new`を見ます。

```rust
let mut keyed: Vec<(Vec<Value>, Tuple)> = Vec::new();
while let Some(tuple) = input.next()? {
    ctx.cancel.check()?;
    let row = Row::new(&schema, &tuple);
    let key: Vec<Value> =
        keys.iter().map(|k| eval_bound_expr(&k.expr, functions, Some(&row))).collect::<DbResult<Vec<_>>>()?;
    keyed.push((key, tuple));
    ctx.check_row_limit("Sort", keyed.len())?;
}
```

`Hash Join`のBuild側(`HashJoinExec::new`)、`Hash Aggregate`(`HashAggregateExec::new`)、`Nested Loop Join`の右側の事前収集(`NestedLoopJoinExec::new`)にも、同じ形で`check`を差し込んであります。

冒頭のクエリ(`a.id = a.id`という常に真の条件を持つ`JOIN`)がキャンセルされる仕組みを、これで説明できます。
`a.id = a.id`は列参照同士の比較なので等値結合として認識されますが、鍵の値そのものは`orders`側の行ごとに変わるため、`Nested Loop Join`が選ばれるとします。
右側のテーブルを`NestedLoopJoinExec::new`が事前に読み切る段階、そして最上位の駆動ループが1行返すたびに、`check`が呼ばれます。
どちらの箇所でキャンセル要求に気付いても、そこで安全に打ち切れます。

`ctx`は`crate::cancellation::ExecutionContext`という、`CancellationToken`と`max_operator_rows`(メモリ上限、後述)をまとめた型です。

```rust
pub struct ExecutionContext {
    /// キャンセル・タイムアウトの合図。
    pub cancel: CancellationToken,
    /// `Sort`・`Hash Join`のBuild側・`Hash Aggregate`が子から集める行数の上限。
    /// `None`なら無制限(この章より前の挙動のまま)。
    pub max_operator_rows: Option<usize>,
}
```

`Database::execute`(第9章以来の低レベルAPI)は、この`ExecutionContext::unbounded()`(キャンセルもタイムアウトもメモリ上限も課さない、この章より前の挙動そのもの)を使い続けます。
`Session`(第37章)経由で実行する文だけが、実際にキャンセル・タイムアウト・メモリ上限の対象になります。
既存の数百件のテストが`db.execute("...")`という形のまま変更無く緑のままなのは、この既定値の分離によるものです。

## クエリキャンセル: 2つの経路

`CancellationToken`へキャンセル要求を立てる経路は2つあります。

1つは、クライアントが接続を切ったことをサーバーが検知する経路です。
`crate::server`は、文を1本実行している間、`stream.try_clone()`で複製した読み取り専用のソケットを別スレッドで`peek`し続けます。

```rust
fn watch_for_disconnect(stream: TcpStream, done: &AtomicBool, cancel: &CancellationToken) {
    if stream.set_read_timeout(Some(POLL_INTERVAL)).is_err() {
        return;
    }
    let mut probe = [0u8; 1];
    loop {
        if done.load(Ordering::Acquire) {
            return;
        }
        match stream.peek(&mut probe) {
            Ok(0) => {
                cancel.cancel();
                return;
            }
            // 1バイト以上読めた場合は切断ではない。パイプライン化されたクライアント
            // (次のリクエストを先読みで送ってくる)を誤ってキャンセルしないよう、
            // データの中身は見ず読み進めもせずにポーリングを続ける。
            Ok(_) => continue,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(_) => {
                cancel.cancel();
                return;
            }
        }
    }
}
```

`peek`はバイト列を消費しないため、この監視スレッドが読んだつもりのバイトが本来のリクエスト処理を妨げることはありません。
`peek`が`0`バイトを返すのは、相手が接続を正常に閉じた(TCPのFIN)ときです。
1バイト以上読めた場合はまだ切断ではない(パイプライン化されたクライアントが次のリクエストを先に送ってきただけかもしれない)ため、中身を見ずに読み進めもせず、ポーリングを続けます。

もう1つは、Rustの公開APIとして`Session::cancellation_handle`を呼ぶ経路です。

```rust
pub fn cancellation_handle(&self) -> CancellationToken {
    CancellationToken::with_checkpoints(Arc::clone(&self.cancel_flag), None, Arc::clone(&self.checkpoints))
}
```

`&self`だけで呼べるので、`session.execute(...)`(`&mut self`を要求し、実行が終わるまで戻ってこない)を別スレッドへ渡す前に取得しておけます。
埋め込み用途のコードやテストは、この`CancellationToken`を持ったまま別スレッドから`cancel()`を呼ぶだけで、実行中の文を打ち切れます。

明示的なキャンセル要求を、この章のWire Protocol(第36章)へ専用のメッセージ種別として追加する案も検討しました。
PostgreSQLは実際にこれを、実行中の接続とは別のTCP接続、共有のシークレットキーという形で実現しています(同じ接続の中では、応答を待っている間はリクエストを送れないため、"今すぐ止めて"を送る経路そのものが無いのです)。
この教材ではその2本目の接続とシークレットキーの受け渡しという新しい概念を追加する代わりに、接続の切断そのものをキャンセルの合図として扱う設計を選びました。
「クライアントが処理を諦めたら、サーバーもそれ以上計算を続ける理由が無い」という単純な原則で説明できる範囲に留め、専用メッセージによる明示的キャンセルは章末の演習課題に譲ります。

## 接続数を固定するワーカープール

キャンセルが効くようになっても、接続を受け付けるたびにスレッドを立て続ける限り、同時に処理できる接続数はクライアントの都合次第のままです。
この章は`crate::thread_pool::WorkerPool`という、固定サイズのワーカースレッドの集合へ置き換えます。

```rust
pub struct WorkerPool {
    /// `None`になるのは[`WorkerPool::join`]がSenderを`drop`した後だけ
    /// (モジュール冒頭「新規ジョブの受付を止める」を参照)。
    sender: Option<SyncSender<TcpStream>>,
    workers: Vec<JoinHandle<()>>,
}
```

`std::sync::mpsc::sync_channel(queue_capacity)`が、この章のキューです。
容量固定のバウンデッドチャネルで、容量を超えて`try_send`すると`Err(TrySendError::Full)`を返します。

```rust
pub fn dispatch(&self, stream: TcpStream) -> Result<(), TcpStream> {
    let sender = self.sender.as_ref().expect("shutdown後にdispatchは呼ばれない(Server::runの契約)");
    match sender.try_send(stream) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(stream)) => Err(stream),
        Err(TrySendError::Disconnected(_)) => {
            unreachable!("selfがsenderを保持している間、receiver側のworkerは必ず生きている")
        }
    }
}
```

`worker_count`本のワーカーが全員別の接続を処理中のとき、新しい接続をどう扱うかには2つの選択肢があります。
キューに積んで**待たせる**か、**即座に拒否する**かです。

この章は拒否を選びます。
`queue_capacity`個までは待たせますが、それも埋まっていれば`Err`を返し、`crate::server`はその接続へエラー応答を1つ書いてから切断します。

```rust
fn reject_connection(mut stream: TcpStream) {
    let response = Response::Error("接続数が上限に達しています。しばらくしてから再接続してください。".to_string());
    let _ = response.write(&mut stream, 0);
}
```

対する「待たせる」設計(`queue_capacity`を実質無制限にする)は、接続数の上限という当初の目的を弱めます。
同時に1,000個の接続が殺到すれば、ワーカーが何本に固定されていても、999個の`TcpStream`がキューの中で待ち続け、結局「同時に保持するリソースの数」は無制限のまま残ってしまいます。
即座に拒否すれば、クライアントは待ち続けて何が起きているか分からないまま固まるより先に「今は繋がらない」と知り、必要なら自分の判断で再接続を試みられます。
`worker_count + queue_capacity`が、この章のサーバーが同時に保持する接続数の実質的な上限になります。

`WorkerPool::new`に渡す`handler`は`Fn`(`FnOnce`ではありません)です。
1本のワーカースレッドは生きている間に何本もの接続を順に処理するため、`handler`はワーカーの数だけ複製されるのではなく、`Arc`で全ワーカーに共有されます。

```rust
pub fn new<F>(worker_count: usize, queue_capacity: usize, handler: F) -> Self
where
    F: Fn(TcpStream) + Send + Sync + 'static,
{
    assert!(worker_count > 0, "ワーカースレッドは最低1本必要です");
    let (sender, receiver) = sync_channel::<TcpStream>(queue_capacity);
    let receiver = Arc::new(Mutex::new(receiver));
    let handler = Arc::new(handler);

    let workers = (0..worker_count)
        .map(|_| {
            let receiver = Arc::clone(&receiver);
            let handler = Arc::clone(&handler);
            thread::spawn(move || worker_loop(&receiver, handler.as_ref()))
        })
        .collect();

    WorkerPool { sender: Some(sender), workers }
}
```

`std::sync::mpsc::Receiver`は複数スレッドで共有できない(`!Sync`)型なので、受信側を`Mutex`で包み、`worker_count`本のワーカースレッドがこの`Mutex`を奪い合って次の接続を受け取ります。

## 締め切りとしてのタイムアウト

`CancellationToken`はすでに`deadline: Option<Instant>`を持っていました。
文単位の実行時間の上限(Query Timeout)は、この`deadline`を使うだけで実装できます。
新しい仕組みを1つも足す必要はありません。

締切は`crate::database::ResourceLimits`が持つ`statement_timeout`から作ります。

```rust
pub struct ResourceLimits {
    /// 1文が実行に使ってよい時間の上限。`None`なら無制限。
    pub statement_timeout: Option<std::time::Duration>,
    /// `Sort`・Hash JoinのBuild側・Hash Aggregateが集めてよい行数の上限。
    /// `None`なら無制限。
    pub max_operator_rows: Option<usize>,
}
```

`Session`は文を1本実行するたびに、`SharedDatabase`に設定された`ResourceLimits`を元に新しい`ExecutionContext`を作ります。

```rust
fn new_execution_context(&self) -> crate::cancellation::ExecutionContext {
    self.cancel_flag.store(false, std::sync::atomic::Ordering::Relaxed);
    self.checkpoints.store(0, std::sync::atomic::Ordering::Relaxed);
    self.shared.make_execution_context(Arc::clone(&self.cancel_flag), Arc::clone(&self.checkpoints))
}
```

`cancel_flag`を`false`へ戻すのは、前の文へのキャンセル要求が次の文へ漏れないようにするためです。
`deadline`は締切そのものなので毎回作り直す必要があり、`CancellationToken::new`(または`with_checkpoints`)を呼ぶたびに`Instant::now() + timeout`として計算し直します。

**時刻をどこで計測するか**は、この章がもう1つ決めた設計判断です。
締切は「`Session::execute`が呼ばれた瞬間」から数えます。
構文解析、束縛、計画の最適化(第26〜29章)はすでに済んでいることが多い高速な処理ですが、`PREPARE`済みの文を`EXECUTE`する場合はこれらを一切やり直しません(第37章)。
実行に無関係な準備段階の時間差を締切へ含めるかどうかで挙動が割れるのを避け、「文の実行を頼んでから、実際に打ち切られるまで」という利用者から見える時間で統一しています。

`statement_timeout`をサーバーの起動引数として指定できるようにしてあります(`--statement-timeout-ms`)。
`SET`文のような実行時のSQL構文は追加しませんでした。
`statement_timeout`、`max_operator_rows`は接続(セッション)ごとに変える理由が薄く、「このサーバープロセスがどれだけの資源を1文に許すか」という運用ポリシーの性質が強いという判断です。

## 収集バッファのメモリ上限

`Sort`、`Hash Join`のBuild側、`Hash Aggregate`は、子から読んだ行を`Vec`やハッシュテーブルへすべて溜め込んでから結果を返す、blocking演算子です。
上限の無いテーブルを`ORDER BY`すれば、この収集バッファはテーブルの全行分そのままメモリに載ります。

`ExecutionContext::check_row_limit`が、この上限を検査します。

```rust
pub fn check_row_limit(&self, operator: &'static str, rows: usize) -> DbResult<()> {
    if let Some(limit) = self.max_operator_rows
        && rows > limit
    {
        return Err(DbError::MemoryLimitExceeded { operator, limit });
    }
    Ok(())
}
```

`SortExec::new`は1行溜めるたびにこれを呼びます(前掲のコードの`ctx.check_row_limit("Sort", keyed.len())?`)。
`Hash Join`のBuild側は挿入した行数、`Hash Aggregate`は作られた別々のグループの個数を数えます。

上限を**行数**で決めているのは、**概算バイト数**という選択肢との比較の結果です。
バイト数で上限を決めるには、`Tuple`が持つ`TEXT`列の実際の長さまで辿って足し合わせる必要があり、行を1件処理するたびにそのコストがかかります。
行数であれば、すでに持っている`Vec::len()`や`HashMap`のグループ数を比べるだけで済みます。
この章は単純さを優先して行数を選びましたが、同じ行数でも`TEXT`列の長さ次第で実際のメモリ使用量は大きく変わりうるため、正確な見積もりが必要な場面では不十分です(章末の演習課題)。

外部ソート、外部Hash Join(メモリに載り切らない入力を一時ファイルへのスピルで処理する手法)は、この上限に触れたときの代替手段として実在しますが、この教材の対象規模を超える発展的な話題として発展編C(実行エンジン)に譲ります。
この章のメモリ上限は、「スピルして続行する」のではなく「上限を超えたら諦めて`DbError::MemoryLimitExceeded`を返す」という、単純だが正直な割り切りです。

## Graceful Shutdownの手順

`Ctrl-C`を受けたサーバーが、次の順序で止まります。

1. **新規接続の受付停止**
2. **実行中の文の完了(またはキャンセルによる打ち切り)**
3. **未コミットTxのROLLBACK**
4. **flush/sync**

`Server::shutdown_handle`が返す`ShutdownHandle`の`trigger`が、この手順の起点です。

```rust
pub fn trigger(&self) {
    self.flag.store(true, Ordering::Release);
    // `accept`でブロック中のServer::runを起こす自己接続トリック
    // (モジュール冒頭を参照)。接続を受理させたいだけなので、返る
    // `TcpStream`はすぐdropしてよい。すでにlistenerが閉じていれば
    // (二重shutdown、またはrunがすでに戻っている)このconnectは
    // 失敗するが、その場合はもうaccept自体が残っていないので無視してよい。
    let _ = TcpStream::connect(self.local_addr);
}
```

フラグを立てただけでは、`TcpListener::accept`でブロックしたままのスレッドは起きません。
`trigger`は、フラグを立てた直後に自分自身のアドレスへ`TcpStream::connect`します(**自己接続トリック**)。
これで`accept`がその接続を1回だけ受理して戻り、`Server::run`のループの先頭でフラグを確認して抜けられます。

```rust
loop {
    match listener.accept() {
        Ok((stream, _)) => {
            if shutdown.load(Ordering::Acquire) {
                // 自己接続トリック(`ShutdownHandle::trigger`)自身の接続、
                // または受付停止の直前に滑り込んだ接続。どちらも処理せず
                // 閉じる。
                drop(stream);
                break;
            }
            if let Err(rejected) = pool.dispatch(stream) {
                reject_connection(rejected);
            }
        }
        Err(err) => {
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            return Err(err);
        }
    }
}

pool.join();
shared.flush().map_err(std::io::Error::other)?;
Ok(())
```

`pool.join()`が、手順2と3を実質的に担います。
`WorkerPool::join`はSenderを`drop`し、それぞれの接続の処理ループが次のリクエストを待つ間もワーカースレッドを離れずにいます。

```rust
pub fn join(mut self) {
    self.sender.take();
    for worker in self.workers.drain(..) {
        let _ = worker.join();
    }
}
```

`crate::server::handle_connection`の読み取りループは、次のフレームが届く前に定期的にシャットダウンフラグを確認します。

```rust
fn wait_for_request_or_shutdown(stream: &mut TcpStream, shutdown: &AtomicBool) -> WaitOutcome {
    if stream.set_read_timeout(Some(POLL_INTERVAL)).is_err() {
        return WaitOutcome::Disconnected;
    }
    let mut probe = [0u8; 1];
    loop {
        if shutdown.load(Ordering::Acquire) {
            return WaitOutcome::Shutdown;
        }
        match stream.peek(&mut probe) {
            Ok(0) => return WaitOutcome::Disconnected, // 相手が正常に閉じた(EOF)
            Ok(_) => {
                let _ = stream.set_read_timeout(None);
                return WaitOutcome::Ready;
            }
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(_) => return WaitOutcome::Disconnected,
        }
    }
}
```

`peek`はバイト列を消費しないため、ポーリングのタイムアウトがフレームの途中で発生しても、次の`Request::read`はフレームの先頭から読み直せます。
1バイト以上届いていることを確認できた時点で読み取りタイムアウトを外し(`set_read_timeout(None)`)、以後は通常どおりブロッキングで読みます。
文が実行中であれば、その文は打ち切られる(キャンセルされる)か、正常に終わるまで続きます。
接続の処理ループが戻れば、その接続の`Session`がスコープを抜け、`Drop`実装が保持中のトランザクションを`ROLLBACK`します(手順3)。
すべての接続がここまで進んで`WorkerPool::join`が戻れば、最後に`SharedDatabase::flush`を呼びます(手順4)。

REPL(`src/main.rs`)がすでに終了時に`shared.flush()`を呼んでいたのと同じ契約を、サーバーでも守っていることになります。
違いは、REPLが単一スレッドの制御フローの終わりに直接`flush`を呼ぶだけなのに対し、サーバーは複数の接続スレッドの終了を`WorkerPool::join`で待ち合わせてから同じことをする点です。

**実際のSIGINTをどう受け取るか**も、この章が決めた設計判断です。
シグナルハンドラの中で呼んでよい処理は、OSのシグナル配送の一般的な制約により、シグナルセーフな一部の関数に限られます。
この教材は生の`libc`シグナルハンドラを自作せず、`ctrlc`クレートに任せます。
`ctrlc`はハンドラの中で安全な操作だけを行い、実際のシャットダウン処理(`Server::run`のループがフラグを見て抜ける)はシグナルハンドラの外、通常のスレッドの上で進みます。

```rust
let shutdown = server.shutdown_handle();
if let Err(e) = ctrlc::set_handler(move || shutdown.trigger()) {
    eprintln!("警告: Ctrl-Cハンドラを登録できませんでした({e})。Graceful Shutdownは内部APIからのみ利用できます。");
}
```

`ShutdownHandle::trigger`をシグナルハンドラの外から直接呼べるようにしてあるおかげで、テストコードは実際にシグナルを送る代わりに同じ`trigger`を呼んでGraceful Shutdownの手順を検証できます。

## テスト

`src/thread_pool.rs`の単体テストは、`WorkerPool`単体の振る舞いを次の観点で確認します。

- `dispatch`したジョブがワーカースレッド上で実行される
- ワーカー1本、キュー容量0のプールを、1本のハンドラが解放されるまで塞いでおくと、その間の`dispatch`は`Err`で拒否される(N+1本目の挙動)
- `join`は、キュー済みのジョブも含めてすべて処理し終えてから戻る

`queue_capacity`が`0`のプールへの`dispatch`は、ワーカースレッドがすでに`recv`で待っていることを前提にしたレンデブー方式です。
`WorkerPool::new`が返った直後はスレッドの起動がまだ完了していないことがあるため、テストは`sleep`ではなく、ワーカーが実際に受け取るまで`dispatch`をリトライする形で待ちます。

`src/server.rs`の単体テストは、実際に`TcpListener`をbindしてTCP接続越しに確認します。

- ワーカー1本、キュー容量0のサーバーに接続を1本張ると、それだけで唯一のワーカーを占有し続ける(接続の処理ループは次のリクエストを待つ間もワーカースレッドを離れないため)。その間の2本目の接続は拒否応答を受け取り、1本目が切断すれば3本目は通常どおり処理される
- `ShutdownHandle::trigger`(シグナルの代わりの内部API)を呼ぶと、未コミットのトランザクションがROLLBACKされ、`Server::run`が戻り、ファイルをflush済みの状態で再オープンできる

`src/session.rs`の単体テストは、`Session::cancellation_handle`と`ResourceLimits`を次の観点で確認します。

- 別スレッドから取得した`cancellation_handle`を`cancel`すると、実行中の長いクエリが`DbError::QueryCancelled`で打ち切られる
- `statement_timeout`を短く設定すると、締切に対して十分長くかかるクエリが`DbError::QueryTimeout`で打ち切られる
- `max_operator_rows`を小さく設定すると、`Sort`、`Hash Join`のBuild側、`Hash Aggregate`がそれぞれ`DbError::MemoryLimitExceeded`で打ち切られ、上限内の行数では正常に完走する

キャンセルのテストは、`sleep`による時間待ちを避けています。

```rust
let handle = session.cancellation_handle();
let worker = std::thread::spawn(move || {
    // ...
    session.execute("SELECT * FROM t AS a JOIN t AS b ON 1 = 1")
});

// ...
handle.wait_for_checkpoints(1_000);
handle.cancel();

let result = worker.join().expect("ワーカースレッドがpanicした");
assert!(matches!(result, Err(DbError::QueryCancelled)), "{result:?}");
```

`wait_for_checkpoints`は、`CancellationToken::check`が呼ばれた回数が指定の閾値に達するまでスピンウェイトします。

```rust
pub fn wait_for_checkpoints(&self, at_least: usize) {
    while self.checkpoints() < at_least {
        std::hint::spin_loop();
    }
}
```

`t`が300行のテーブルであれば、`t AS a JOIN t AS b`の総組み合わせは9万通りです。
1,000回という閾値は総組み合わせよりはるかに小さいため、この閾値へ到達した時点でクエリがまだ実行の途中であることが保証されます。
`sleep(Duration::from_millis(N))`のような固定時間の待機であれば、実行環境の速度によっては「待っている間にクエリ自体が終わってしまい、キャンセルされる前に正常終了する」という偽陽性(キャンセルを試したつもりが何も検証していないテスト)が起こりえます。
実際の進捗(`checkpoints`)を見て待つことで、この偽陽性を構造的に避けています。

タイムアウトのテストは対照的に、時間そのものを検証対象にしているため`sleep`を避けようがありません。
その代わり、締切(1ミリ秒)とクエリの実行時間(700×700=49万通りの組み合わせを評価する、実測で数十ミリ秒以上)の比を十分大きく取ることで、実行環境の速度差による揺れを吸収しています。

## この章の限界

ロックを待っている間(`DbError::WouldBlock`の再試行、`SharedDatabase`の`Condvar::wait`)は、キャンセル・タイムアウトの対象にしていません。
文の実行そのものはまだ始まっておらず、`CancellationToken::check`を呼ぶ機会が無いためです。
ロックを長時間待たされているクライアントを、待っている間に諦めさせる仕組みは章末の演習課題に譲ります。

`INSERT`、`UPDATE`、`DELETE`、DDLはこの章のキャンセル、タイムアウト、メモリ上限の対象外です。
`run_bound_statement`は`ExecutionContext`を`SELECT`(`execute_select`、`execute_explain`)にしか渡していません。
`UPDATE`、`DELETE`も`Storage::scan`を先頭から読み進める全件走査を内部に持ちますが、この章はそこまで手を広げていません。

明示的なキャンセル要求をWire Protocolのメッセージとして送る経路は追加していません。
接続を切ることでしか、クライアントの側から実行中のクエリを諦めさせられません。

`max_operator_rows`は行数で上限を決めており、`TEXT`列の実際の長さは見ていません。
1,000行という同じ上限でも、短い整数列だけのテーブルと、1行1MiBの`TEXT`列を持つテーブルとでは、実際のメモリ使用量は大きく異なります。

## 演習問題

### 必須課題

1. `crate::database::Database::run_update`、`run_delete`は`ExecutionContext`を受け取らないため、`WHERE`句の無い`DELETE FROM huge_table`のような文はこの章のキャンセル、タイムアウトの対象になりません。`executor::storage_delete`、`executor::delete`のループへ`ctx: &ExecutionContext`を足し、`ctx.cancel.check()?`を差し込むとしたら、シグネチャの変更はどこまで波及するか(`run_bound_statement`から`execute_delete`までの呼び出し経路)を実際に洗い出してください。
2. `WorkerPool`の`queue_capacity`を`0`にした場合と`4`にした場合とで、`src/server.rs`の`the_n_plus_first_connection_is_rejected_while_the_single_worker_is_busy`テストの前提(2本目の接続が即座に拒否される)がどう変わるか、実際に`queue_capacity`を変えて手元で確認してください。
3. `wait_for_request_or_shutdown`の`POLL_INTERVAL`(50ミリ秒)を極端に長く(たとえば5秒に)変えると、`graceful_shutdown_rolls_back_and_flushes_before_run_returns`テストの実行時間にどう影響するか予想し、実際に変更して確認してください。Graceful Shutdownの応答性とポーリングのコストのトレードオフを、この観測から言葉にしてください。

### 発展課題

1. `CancellationToken`に、`WouldBlock`の再試行ループでも`check`を呼べるようにする変更を設計してください。`SharedDatabase::execute_in_tx_bound`の`Condvar::wait`は無期限に眠るため、`Condvar::wait_timeout`へ置き換えて定期的に起こし、その都度`ctx.cancel.check()`するという方針が考えられます。この変更が、`Condvar::notify_all`に頼ってきた既存の再試行の規律(第31章)とどう共存できるか検討してください。
2. `max_operator_rows`ではなく概算バイト数で上限を決める設計に変更してください。`Value`ごとの概算サイズ(`BigInt`は8バイト、`Boolean`は1バイト、`Text`は文字列の長さ)を返す関数を`crate::types`に追加し、`Sort`、`Hash Join`、`Hash Aggregate`の収集バッファがその合計を追跡するように変更したうえで、行数上限との挙動の違いを実際のテストで示してください。
3. PostgreSQLに倣い、専用の2本目のTCP接続と共有のシークレットキーで実行中のクエリを明示的にキャンセルできる仕組みを設計してください。`crate::protocol`へ新しいメッセージ種別(`MSG_CANCEL`)を追加し、`crate::server`がどの接続の`CancellationToken`を対象にするかをどう特定するか(接続ごとの識別子とシークレットキーの対応表をどこに持つか)を検討し、実装してください。
