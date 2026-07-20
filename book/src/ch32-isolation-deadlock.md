# 第32章 Isolation LevelとDeadlock

第31章の最後に`src/lock_manager.rs`へ書いたテスト`mutual_wait_leaves_both_transactions_blocked_without_detection`を、もう一度見てみます。

```rust
assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
assert_eq!(lm.acquire(T2, table(2), LockMode::Exclusive), LockResult::Granted);

// T1はT2の持つtable(2)を、T2はT1の持つtable(1)を欲しがる。
assert_eq!(lm.acquire(T1, table(2), LockMode::Exclusive), LockResult::Blocked);
assert_eq!(lm.acquire(T2, table(1), LockMode::Exclusive), LockResult::Blocked);
```

T1はT2の持つ`table(2)`を、T2はT1の持つ`table(1)`を欲しがっています。
このテストが確認していたのは、この状態が実際に起こることだけでした。
この先どうなるのか、という問いにはまだ誰も答えていません。

Blockedになった要求は、対象のロックが解放されるたびに再評価されます(第31章の`promote_waiters`)。
ところがT1の持つ`table(1)`が解放されるのはT1が`COMMIT`か`ROLLBACK`したときであり、そのT1自身は`table(2)`が空くのを待っています。
T2も同じ立場です。
どちらも相手が先に動くのを待っており、相手も同じことをしています。
このままでは、T1もT2も二度と`COMMIT`にたどり着けません。

## 前章に残った2つの宙ぶらりん

上の相互待ちが、この章が引き取る1つ目の宙ぶらりんです。
第31章の`LockManager`は、ロックが取れない要求を待ち行列に積むところまでしか面倒を見ません。
積んだ要求同士が輪になって互いを待ち合っていても、それに気づく仕組みがどこにもありませんでした。

2つ目は`Backend::Disk`(Tuple Lock)に残ったPhantomです。
第31章は`SELECT`のロックを「その時点で存在する行」だけに掛けました。
`tests/interleave_disk.rs`に書いた次のテストが、それを示しています。

```rust
#[test]
fn phantom_read_is_not_prevented_by_tuple_lock() {
    // ...
    // Table Lock(`tests/interleave.rs`)と違い、T2のINSERTはブロックされない。
    db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)").unwrap();
    db.commit_tx(t2).unwrap();

    let second = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let second_count = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_count, 3, "T2が挿入した行(幻)が2回目の集計に現れている(Phantomはまだ起きる)");
}
```

新しく挿入される行の`RecordId`は、挿入が終わるまで存在しません。
存在しないものはロックできないので、T1の集計中にT2が新しい行を差し込んでも、誰もそれを止めません。
`Backend::Memory`(Table Lock)ではこの非対称性自体が無く、4つの異常はすべて防がれていました。
粒度を細かくした代償として、Diskバックエンドだけがこの1点を取りこぼしたままだったのです。

この2つは無関係な話ではありません。
相互待ちを解く仕組みを持たないままロックの粒度をさらに細かくしていくと、デッドロックはますます起こりやすくなります。
逆に言えば、この章でPhantomを塞ぐ手を考えるより先に、待ち合いをそのまま放置しないための仕組みを用意しておく必要があります。
この章はデッドロックの検出から先に固め、そのうえでPhantomの残りを片付けます。

## 分離レベルというダイヤル

ここまでの`LockManager`は、Shared LockもCOMMITまで手放さないという1種類の規律しか知りませんでした。
これはSQL標準が定める4つの**分離レベル**(Isolation Level)のうち、`REPEATABLE READ`に相当する規律です。
標準はこの他に`READ UNCOMMITTED`、`READ COMMITTED`、`SERIALIZABLE`を定めており、それぞれ「読み取りロックをどこまで律儀に取るか」が異なります。

分離レベルを上げるほど、防げる異常は増えますが、その代わりトランザクションはより頻繁に他人の作業を待つようになります。
下げれば逆に、待たされる回数は減りますが、Dirty ReadやLost Updateのような異常を許すことになります。
どちらが正しいかは、アプリケーションが何を求めているかで決まる話であり、データベース自身が一方的に決めてよい話ではありません。
分離レベルは、この性能と正しさの取引をユーザー自身に委ねるための**ダイヤル**です。

SQL標準は各レベルを「どの異常を許すか」で定義しており、実装の中身(ロックなのかMVCCなのか)までは指定しません。
このクレートはロックだけで並行制御を作ってきたので、4つのレベルもロックの規律の違いとして実装します。
書き込み側の規律(Exclusive Lockを`COMMIT`まで持ち越すStrict 2PL)はどのレベルでも変えません。
変えるのは常に、読み取りにどこまでロックを効かせるかだけです。

## BEGIN文に分離レベルを持たせる

`BEGIN`単体しか受理していなかった構文に、`BEGIN ISOLATION LEVEL <level>`を追加します。
`src/ast.rs`に次の`BeginStatement`と`IsolationLevel`を定義します。

```rust
/// `BEGIN`文(第30章)。`BEGIN TRANSACTION`のような修飾は持たず、`BEGIN`
/// 単体、または`BEGIN ISOLATION LEVEL <level>`(第32章)だけを受理する
/// (`docs-local/chatgpt_opinion.md`の原案が挙げる`BEGIN TRANSACTION`・
/// `BEGIN WORK`のような修飾語は、この章でも引き続き受理しない)。
#[derive(Debug, Clone, PartialEq)]
pub struct BeginStatement {
    /// 省略した場合は`None`になり、`Database::execute_begin`が既定の
    /// 分離レベル(Repeatable Read、本文「分離レベルの既定値」を参照)を補う。
    pub isolation_level: Option<IsolationLevel>,
    pub span: Span,
}

/// `BEGIN ISOLATION LEVEL ...`(第32章)が指定できる4つの分離レベル。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// `READ UNCOMMITTED`。読み取りロックを一切取らない。
    ReadUncommitted,
    /// `READ COMMITTED`。読み取りロックを文の終わりで解放する。
    ReadCommitted,
    /// `REPEATABLE READ`。読み取りロックもCOMMITまで保持する(第31章の
    /// Strict 2PLがもともと持っていた挙動そのもの)。
    RepeatableRead,
    /// `SERIALIZABLE`。`RepeatableRead`に加え、Phantomも防ぐ。
    Serializable,
}
```

対応する原案(`docs-local/chatgpt_opinion.md`)は`SET TRANSACTION ISOLATION LEVEL`と`BEGIN [ISOLATION LEVEL ...]`のどちらも例に挙げていますが、この章は後者だけを実装します。
`SET TRANSACTION`は「次に始まる1本のトランザクションだけに効く設定文」という独立した意味を持ち、実行順序の扱い(`BEGIN`の前に書くのか後に書くのか、`Active`でない間にしか効かないのか)を新たに設計する必要があります。
`BEGIN`に直接持たせれば、分離レベルは`TransactionContext`が生まれる瞬間に確定し、`Active`の途中で変わりうる余地そのものが最初から無くなります。
構文を1つ増やす代わりに、状態遷移の分岐を1つ減らせる選択です。

`Parser`はこの1個のオプション句を読み取るだけです。
`src/parser.rs`に次の`parse_begin_statement`を書きます。

```rust
fn parse_begin_statement(&mut self) -> DbResult<BeginStatement> {
    let start = self.expect_keyword(Keyword::Begin, "BEGIN")?;
    if !matches!(self.peek_kind(), TokenKind::Keyword(Keyword::Isolation)) {
        return Ok(BeginStatement { isolation_level: None, span: start });
    }
    self.advance();
    self.expect_keyword(Keyword::Level, "LEVEL")?;
    let (level, end) = self.parse_isolation_level()?;
    Ok(BeginStatement { isolation_level: Some(level), span: Span { start: start.start, end } })
}
```

`READ`の直後だけ`UNCOMMITTED`と`COMMITTED`のどちらが続くかで分岐し、`REPEATABLE`は必ず`READ`を伴い、`SERIALIZABLE`は単独で完結します。
`Binder`はこの文を素通りさせます(`BEGIN`自体が名前解決すべき対象を持たない、第30章の説明のとおり)。
`IsolationLevel`はASTの一部としてそのまま`Database::execute_begin`まで届き、`TransactionContext`へ書き込まれます。

### 分離レベルの既定値

`BEGIN`単体(`ISOLATION LEVEL`を省略した場合)がどのレベルになるかは、この章が新しく決める必要がある設計判断です。
`src/database.rs`の`execute_begin`を次のように書きます。

```rust
fn execute_begin(&mut self, begin: BeginStatement) -> DbResult<QueryResult> {
    if self.tx.is_some() {
        return Err(DbError::TransactionAlreadyActive);
    }
    let id = TransactionId(self.next_txn_id);
    self.next_txn_id += 1;
    let level = begin.isolation_level.unwrap_or(IsolationLevel::RepeatableRead);
    self.tx = Some(TransactionContext::new(id, level));
    Ok(QueryResult::command("BEGIN"))
}
```

PostgreSQLの既定は`READ COMMITTED`です。
それでもこの章は`RepeatableRead`を選びました。
第31章のStrict 2PLは、`BEGIN`に分離レベルという概念が無いまま、すでに「Shared LockもCOMMITまで手放さない」という規律1本で動いていたからです。
`READ COMMITTED`を既定にすると、第31章までに書いた`BEGIN`を伴うテストと本文の例すべてが、読み取りロックを文の終わりで解放する挙動へ無言で意味を変えてしまいます。
明示的に`ISOLATION LEVEL`と書いた場合にだけ、その分離レベルの規律に従わせることで、この章より前の章の挙動を1つも壊さずに済みました。
実際、第30章と第31章が積み上げてきた`tests/interleave.rs`と`tests/interleave_disk.rs`は、この章での変更を1行も加えずに全テスト緑のまま残ります。

決定的インターリーブテストハーネスにも、同じ既定値を持つAPIを用意します。

```rust
pub fn begin_tx(&mut self) -> TxHandle {
    self.begin_tx_with_isolation(IsolationLevel::RepeatableRead)
}

pub fn begin_tx_with_isolation(&mut self, isolation_level: IsolationLevel) -> TxHandle {
    let id = TransactionId(self.next_txn_id);
    self.next_txn_id += 1;
    self.harness_contexts.insert(id, TransactionContext::new(id, isolation_level));
    TxHandle(id)
}
```

`begin_tx`は`begin_tx_with_isolation(RepeatableRead)`を呼ぶだけの薄い委譲であり、第30章と第31章のテストコードは1文字も変更せずにそのまま動きます。
他のレベルを試したいテストだけが、新しい`begin_tx_with_isolation`を使います。

## 4つの分離レベルをロック規律として実装する

分離レベルごとの違いは、`SELECT`が読み取りロックをどう扱うかに集約されます。
まず、あるトランザクションが今どの分離レベルにいるかを調べる関数が要ります。

```rust
fn isolation_level_of(&self, owner: TransactionId) -> IsolationLevel {
    if let Some(tx) = &self.tx
        && tx.id == owner
    {
        return tx.isolation_level;
    }
    if let Some(ctx) = self.harness_contexts.get(&owner) {
        return ctx.isolation_level;
    }
    IsolationLevel::RepeatableRead
}
```

`owner`が通常のSQL経路の`self.tx`かハーネスの`harness_contexts`のどちらかに見つかれば、そのトランザクションが持つ分離レベルをそのまま返します。
どちらにも見つからない場合、`owner`はAutocommit用に`lock_owner`がその場で割り当てた一時IDです。
`TransactionContext`自体が存在しないこの場合は`RepeatableRead`を返しますが、これは第31章までの挙動をそのまま保つための既定であり、実際にはAutocommitの1文は文の終わりに`execute_bound_statement`がロックを一括で手放すため、`RepeatableRead`か`ReadCommitted`かで結果に違いは出ません。

この`isolation_level_of`を使って、`SELECT`が読み取りロックを取る`acquire_scan_locks`を書き換えます。

```rust
fn acquire_scan_locks(&mut self, owner: TransactionId, table_ids: &[TableId], mode: LockMode) -> DbResult<()> {
    let level = self.isolation_level_of(owner);
    if level == IsolationLevel::ReadUncommitted && mode == LockMode::Shared {
        return Ok(());
    }

    let mut keys = self.scan_lock_keys(table_ids)?;
    if level == IsolationLevel::Serializable && mode == LockMode::Shared && matches!(&self.backend, Backend::Disk { .. })
    {
        keys.extend(table_ids.iter().map(|&id| LockKey::Table(id)));
    }

    for &key in &keys {
        self.acquire_lock_or_detect_deadlock(owner, key, mode)?;
    }

    if level == IsolationLevel::ReadCommitted && mode == LockMode::Shared {
        let newly_acquired = self.lock_manager.take_pending_shared_grants(owner);
        self.lock_manager.release_keys(owner, &newly_acquired);
    }
    Ok(())
}
```

この関数は`mode`が`Shared`(読み取り)のときだけ分離レベルを見ます。
`mode`が`Exclusive`(書き込み)のとき、つまり`INSERT`(Memoryバックエンド)が呼ぶときは、上の`ReadUncommitted`の早期リターンも`ReadCommitted`の即時解放も発動しません。
書き込みロックの規律はStrict 2PLのまま、4つの分離レベルで共通です。

**Read Uncommitted**は、`mode == Shared`の場合に`LockManager`へ触れることすらせず、即座に`Ok(())`を返します。
読み取りロックが無いので、他のトランザクションが持つExclusiveロックと衝突しようがありません。
これがDirty Readを許す理由そのものです。

**Read Committed**は、いったん通常どおりロックを獲得してから、関数を抜ける直前に、この文で**新規に**獲得した`Shared`ロックだけを手放します。
「取ってすぐ返す」ため、Shared Lockを取る瞬間には他のトランザクションの未確定なExclusiveと衝突判定が働き(Dirty Readは防げます)、読み終えたあとは何にも縛られません(次の文の実行時点では、他のトランザクションが自由に書き換えられるため、Non-repeatable Readは防げません)。

「新規に」を強調したのには理由があります。
`owner`が同じトランザクションの先行する`UPDATE`によって、すでにこの鍵にExclusiveロックを持っていることがあります。
このとき`self.lock_manager.acquire`はすでに十分なロックを持っている(`Exclusive`は`Shared`の要求も満たす)と判断してその場で`Granted`を返しますが、これは「新しく獲得した」わけではありません。
このような鍵まで一緒に手放してしまうと、`UPDATE`が確定前の変更を守っていたはずのExclusiveロックを、直後の`SELECT`が同じ行をなぞっただけで解放してしまいます。
そうなれば、`COMMIT`の前にもかかわらず、他のトランザクションがその未確定の行を書き換えられてしまいます。

「新規に獲得した鍵」をどう特定するかには、実は2つの方式を試しました。
最初に書いたのは、鍵ごとに獲得する**前**の保持状況を(`LockManager`に一時的に用意した`held_mode`のようなメソッドで)確認し、「まだ持っていなければ新規」と判定する方式でした。
この方式は、`SELECT`が一度`WouldBlock`で待たされるケースで壊れます。
先行トランザクションのCOMMITによって、待ち行列に並んでいたこのSharedロックが**`owner`がこの文を再試行する前に**昇格していることがあるからです。
再試行した`acquire_scan_locks`が獲得の直前に見る保持状況は、すでに`Some(Shared)`になっており、「以前から持っていた」と誤判定してしまいます。
「獲得する前に尋ねる」というやり方は、獲得(または待ち行列からの昇格)が実際に起きた**タイミング**と、それを尋ねる**タイミング**が一致している前提に頼っており、`WouldBlock`をまたぐ再試行ではその前提が崩れるのです。

採用したのは、「新規に獲得した」という事実そのものを、獲得が実際に起きた瞬間に`LockManager`自身に記録させる方式です。
`src/lock_manager.rs`に次の`take_pending_shared_grants`を追加します。

```rust
pub(crate) fn take_pending_shared_grants(&mut self, txn: TransactionId) -> Vec<K> {
    self.pending_shared_grants.remove(&txn).unwrap_or_default()
}
```

`LockManager`は`pending_shared_grants: HashMap<TransactionId, Vec<K>>`というフィールドを新しく持ちます。
`acquire`が即座に`Granted`を返す瞬間と、`promote_waiters`が待ち行列から昇格させる瞬間のどちらでも、対象が`Shared`であれば`pending_shared_grants[txn]`にその鍵を積みます(`Shared`は`acquire_scan_locks`からしか要求されないため、この記録は常に「`SELECT`が新規に獲得したShared Lock」だけを指します)。
`take_pending_shared_grants`は、この記録を`txn`ごと丸ごと取り出して空にするだけの単純なメソッドです。

この方式なら、`acquire_scan_locks`が獲得を試みた**呼び出しの回数**(`WouldBlock`をまたいで何回再試行したか)によらず、正しく追跡できます。
T1のCOMMITによって待ち行列からT2のSharedが昇格したのが、T2がこの文をまだ一度も再試行していないタイミングだったとしても、`pending_shared_grants[T2]`にはその瞬間に記録が積まれます。
T2が実際に文を再試行して`acquire`を呼んだときは、すでに保持しているので「十分なロックを持っている」分岐(その場で`Granted`)を通るだけで、新たに何かを積む必要はありません。
`acquire_scan_locks`は、獲得ループを終えたあとで`take_pending_shared_grants`を呼ぶだけで、いつ、何回の呼び出しをまたいで昇格したかを気にせず、正しい鍵の集合を受け取れます。

`pending_shared_grants`は`txn`ごとに積み上がる記録なので、`txn`自体が`COMMIT`、`ROLLBACK`、強制Abortで終わるときに片付けておかないと、二度と回収されないエントリが残り続けます。
第31章の`release_all`の末尾に、この後始末を1行加えます。

```rust
pub fn release_all(&mut self, txn: TransactionId) {
    let keys: Vec<K> = self
        .entries
        .iter()
        .filter(|(_, entry)| {
            entry.holders.iter().any(|(t, _)| *t == txn) || entry.waiters.iter().any(|w| w.txn == txn)
        })
        .map(|(key, _)| key.clone())
        .collect();

    for key in keys {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.holders.retain(|(t, _)| *t != txn);
            entry.waiters.retain(|w| w.txn != txn);
            self.promote_waiters(&key);
        }
    }
    self.pending_shared_grants.remove(&txn);
}
```

`release_keys`自体(渡された鍵だけを解放する部分)は前の版から変わっていません。

```rust
pub(crate) fn release_keys(&mut self, txn: TransactionId, keys: &[K]) {
    for key in keys {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.holders.retain(|(t, _)| *t != txn);
            self.promote_waiters(key);
        }
    }
}
```

第31章の`release_all`は`txn`が持つロックを**全部**手放しました。
`release_keys`はそれと違い、渡された`keys`だけを狙い撃ちします。
`Active`なトランザクションの中で`Read Committed`のSELECTを実行しても、そのトランザクションが別の文ですでに獲得しているExclusiveロック(書き込みロック)には一切触れません。
手放すのは、まさに`take_pending_shared_grants`が返した、この文のために新しく取った読み取りロックだけです。

**Repeatable Read**は、この関数に何も特別なことをさせません。
獲得したロックはそのまま残り、`COMMIT`まで保持されます。
第31章の挙動そのものです。

**Serializable**は`RepeatableRead`と同じTuple Lockに加え、`Backend::Disk`の場合だけ`LockKey::Table(table_id)`にも`Shared`を追加で取ります。
この1本がなぜ要るのかは、次の節で説明します。

## SerializableはどうPhantomを防ぐか

第31章が積み残したPhantomの正体は、「まだ存在しない行はロックできない」という一言に尽きます。
Tuple Lockという粒度を保ったまま、まだ存在しない行を予約する手段は2つ考えられます。

1つは**Key Range Lock**です。
`WHERE balance > 40`のような述語が指す**範囲**そのものを1個のロック対象とみなし、その範囲に新しい行を挿入しようとする`INSERT`と衝突させます。
実現できればTuple Lockの細かい粒度を保ったまま、SELECT対象と無関係な範囲への`INSERT`はブロックせずに済みます。
代償は実装の複雑さです。
`WHERE`の述語をロックの対象として表現し、新しい行の値がその範囲に含まれるかを`INSERT`の側でも判定する仕組みが要り、索引の構造(B+Treeのキー順序)とロックの粒度を結びつける設計が必要になります。

もう1つが、この章で選んだ**テーブルロックへの昇格**です。
`Serializable`のSELECTだけ、Tuple Lockに加えてテーブル全体にShared Lockを追加で取ります。
原案(`docs-local/chatgpt_opinion.md`)も「第一版ではTable Lockを使うか、限定的なKey Range Lockを実装する」とこの2択を挙げており、この章は前者を選びました。
Key Range Lockは範囲外の`INSERT`まで巻き込まないぶん並行度が高い一方、この章の`LockManager<LockKey>`は「対象1個」を鍵にする設計であり、範囲をキーにする仕組みは型もアルゴリズムも作り直しになります。
テーブルロックへの昇格なら、すでにある`LockKey::Table`をもう1本追加するだけで済み、この章の分量で実装から検証まで通せます。
捨てたのは、`Serializable`のSELECTと無関係な範囲への`INSERT`まで一律にブロックしてしまうという並行度です。

この追加のTable Lockが刺さる相手が、`INSERT`側の変更です。
`src/database.rs`の`run_insert`を次のように変更します。

```rust
fn run_insert(&mut self, plan: LogicalPlan, owner: TransactionId) -> DbResult<usize> {
    // ...
    if matches!(&self.backend, Backend::Memory { .. }) {
        self.acquire_scan_locks(owner, &[table_id], LockMode::Exclusive)?;
    } else if self.isolation_level_of(owner) == IsolationLevel::Serializable {
        // `Backend::Disk`の`INSERT`は通常どこもロックしない(モジュール
        // 冒頭「ロックの粒度」を参照、新しい行の`RecordId`は挿入が終わる
        // までロックする対象自体が無い)。`Serializable`のときだけ例外で、
        // `acquire_scan_locks`が同じ分離レベルの`SELECT`に取らせる
        // `LockKey::Table`のSharedと衝突させるため、`Exclusive`を先に
        // 取る(本文「SerializableはどうPhantomを防ぐか」を参照)。
        self.acquire_lock_or_detect_deadlock(owner, LockKey::Table(table_id), LockMode::Exclusive)?;
    }
    // ...
}
```

`Backend::Disk`の`INSERT`は、`ReadUncommitted`、`ReadCommitted`、`RepeatableRead`のもとでは第31章のまま何もロックしません。
`Serializable`のときだけ、`LockKey::Table(table_id)`にExclusiveを要求します。
この鍵は、`acquire_scan_locks`が`Serializable`のSELECTに追加させたTable Sharedと**同じ鍵**です。
SELECTが集計を続けている間、そのテーブルのTable SharedはCOMMITまで保持されているため、後から来る`Serializable`のINSERTが要求するTable Exclusiveと衝突し、SELECTが手を放すまで`INSERT`は先に進めません。
既存の行を指すTuple Lockと、テーブル全体を指すTable Lockは元々別々の鍵(`LockManager<LockKey>`の`HashMap`のキーが違う)であり、互いに干渉しません。
Phantomを塞ぐのは、この新しく追加した1本の衝突だけです。

## マトリクステストで確認する

分離レベルの実装が正しいかどうかは、「どのレベルでどの異常が起き、どれが防がれるか」という表と実際のテストの結果が一致することでしか確認できません。
Memoryバックエンド(テーブル単位のロック)でのマトリクスは次のとおりです。

| レベル | Dirty Read | Non-repeatable Read | Lost Update | Phantom |
|---|---|---|---|---|
| Read Uncommitted | 起きる | 起きる | 起きる | 起きる |
| Read Committed | 防がれる | 起きる | 起きる | 起きる |
| Repeatable Read | 防がれる | 防がれる | 防がれる | 防がれる |
| Serializable | 防がれる | 防がれる | 防がれる | 防がれる |

`Repeatable Read`と`Serializable`がMemoryバックエンドで見分けが付かないのは偶然ではありません。
テーブル単位のロックは元から`INSERT`もテーブル全体のExclusiveを要求するため、Phantomの土台(まだ存在しない行への挿入だけが誰にも守られない、というTuple Lockの非対称性)自体が最初から無いのです。
この2レベルの違いが意味を持つのはDiskバックエンド(Tuple Lock)のときだけです。

Lost Updateがこのマトリクスの中で唯一、「読み取りロックを一瞬でも取るかどうか」と「その読み取りロックをいつまで保持するか」の両方に左右される異常だという点に注意してください。
`Read Committed`はSELECTの瞬間にはShared Lockを取りますが、直後に手放します。
`tests/isolation_levels.rs`に書いた次のテストが、それを示します。

```rust
#[test]
fn read_committed_allows_lost_update() {
    // ...
    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t1_seen = int_value(&read, 0, 0);
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t2_seen = int_value(&read, 0, 0);

    db.execute_in_tx(&t2, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t2_seen + 20)).unwrap();
    db.commit_tx(t2).unwrap();

    // T1・T2どちらのSharedロックも読み取りの直後に解放済みなので、T1の
    // UPDATEはT2の書き込みとぶつからず、T1が最初に読んだ100を根拠にした
    // 計算のまま上書きしてしまう。
    db.execute_in_tx(&t1, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t1_seen + 10)).unwrap();
    db.commit_tx(t1).unwrap();

    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&result, 0, 0), 110, "T2の+20がT1の書き込みで上書きされて消えている(Lost Update)");
}
```

T1もT2も、`UPDATE`を実行する時点ではもう自分のSharedロックを持っていません。
`UPDATE`が要求するExclusiveロックは、互いのShared(すでに消えている)ともExclusive(まだ取っていない)とも衝突せず、素通りします。
Lost Updateが防がれるのは`Repeatable Read`以上、つまりSharedロックを`UPDATE`の瞬間まで持ち越すレベルだけです。
`SELECT ... FOR UPDATE`のような、読み取りの時点から書き込み用のロックを明示的に要求する構文を持たないSQLサブセットでは、これがLost Updateを防ぐ唯一の手段になります。

DiskバックエンドのPhantomは、`Repeatable Read`と`Serializable`の違いを直接確認できる唯一の場所です。

```rust
#[test]
fn serializable_prevents_phantom_read_on_disk_backend() {
    let (mut db, path) = accounts_disk_db("isolation-serializable-phantom");
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::Serializable);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::Serializable);

    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    assert_eq!(int_value(&first, 0, 0), 2);

    // RepeatableReadと違い、T2のINSERTはT1のTable Sharedロックとぶつかって
    // ブロックされる。
    assert!(matches!(
        db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)"),
        Err(DbError::WouldBlock)
    ));
    // ...
}
```

`tests/interleave_disk.rs`の`phantom_read_is_not_prevented_by_tuple_lock`(`RepeatableRead`)と見比べると、両者の唯一の違いが`begin_tx`(既定の`RepeatableRead`)を`begin_tx_with_isolation(Serializable)`に変えただけであることが分かります。
分離レベル以外の条件は完全に同じであり、`INSERT`が通るか通らないかだけが反転しています。

```console
$ cargo test --test isolation_levels
test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## デッドロックの検出とVictim Selection

分離レベルを4段階に分けたことで、待ち行列は前章よりも複雑になりました。
ここで章の冒頭に戻ります。
相互待ちに陥ったトランザクションを、いつ、どうやって見つけ出すのでしょうか。

考えられる方式はもう1つあります。
**Timeout**方式です。
ロックを一定時間待っても取れなければ、待っている側を諦めさせて`ROLLBACK`する、という単純な規則で、原案(`docs-local/chatgpt_opinion.md`)もこの方式を選択肢の1つに挙げています。
実装は容易ですが、正しいタイムアウト値を決める手段がありません。
短すぎれば、デッドロックではない単なる混雑(相手が少し時間のかかる処理をしているだけ)まで巻き込んで無駄な`ROLLBACK`を起こします。
長すぎれば、実際に起きたデッドロックを長時間放置します。
このクレートには「時間」という軸そのものがありません(第30章から、複数のトランザクションは実スレッドではなく単一スレッド上の交互実行として進みます)。
Timeoutは`std::time`のような実時間に依存する方式であり、決定的インターリーブテストハーネスの上では「何秒待ったか」を意味のある形で再現できません。
この章がTimeoutではなくWait-for Graphを選んだのは、実装の見通しの良さだけでなく、このクレートの実行モデルと相性が良いという理由もあります。

### Wait-for Graphの組み立て

**Wait-for Graph**は「誰が誰の解放を待っているか」を表す有向グラフです。
トランザクションA→Bの辺は、「Aが要求しているロックを、Bが両立しないモードで保持している」ことを表します。
この辺の集合に閉路(サイクル)があれば、それがそのままデッドロックです。

この辺を組み立てるための生データを、`LockManager`が新しく提供します。
`src/lock_manager.rs`に次の`wait_for_edges`を追加します。

```rust
pub(crate) fn wait_for_edges(&self) -> Vec<(TransactionId, TransactionId)> {
    let mut edges = Vec::new();
    for entry in self.entries.values() {
        for waiter in &entry.waiters {
            for &(holder, held_mode) in &entry.holders {
                if holder == waiter.txn {
                    continue;
                }
                let conflicts = if waiter.is_upgrade { true } else { !held_mode.compatible_with(waiter.mode) };
                if conflicts {
                    edges.push((waiter.txn, holder));
                }
            }
        }
        for pair in entry.waiters.iter().collect::<Vec<_>>().windows(2) {
            let [predecessor, successor] = pair else { unreachable!("windows(2)は常に2要素を返す") };
            edges.push((successor.txn, predecessor.txn));
        }
    }
    edges
}
```

`LockManager`自身はこの先の処理(グラフの探索、Victim Selection)を一切行いません。
第31章から「ロックの獲得と解放」に絞ってきたこの型の責務は変えず、複数のトランザクションをまたいだグラフ探索は1段上の`Database`に置きます。

返す辺は2種類あります。
1つは、要求されたモードと実際に保持されているモードが本物の意味で衝突する辺です。
もう1つが、待ち行列上で自分の直前に並ぶ要求への辺です。
この2つめの辺が無いと、実在する循環待ちを見逃すことがあります。

`promote_waiters`(第31章)は待ち行列を必ず先頭から順に処理し、先頭が昇格できなければそこで止まります。
つまり、ある待ち要求が昇格できるのは、その手前に並ぶすべての要求が先に昇格し終わったときに限られます。
先頭のExclusive要求を追い越せない後続のShared要求も例外ではありません。
その後続のShared要求は、たとえ今の保持者と両立していても、手前の要求が残っている限り追い越して先に昇格することはないのです。

具体的に考えてみます。
T1がテーブルAにSharedロックを持ち、T2がテーブルAにExclusiveを要求してBlockedになったとします(T2→T1、モード衝突による辺)。
続けてT3がテーブルBにExclusiveを獲得したあと、テーブルAにSharedを要求します。
T1の持つSharedとは両立するのですが、待ち行列にはすでにT2が並んでいるため、T3はT2を追い越せずFIFOで後ろに並びます。
ここでT3が両立と衝突判定だけを見ると、T3はT1と衝突していないので辺が生まれません。
しかし実際には、T3はT2が昇格するまで進めないという意味で、T2に依存しています。
最後にT1がテーブルBにExclusiveを要求すると、T3が保持するテーブルBと衝突し(T1→T3)、この時点でT1→T3→T2→T1という循環がすでに実在します。
待ち行列の順序による依存(T3→T2)を辺として持たなければ、このグラフはT1→T3とT2→T1の2本しか持たず、閉路が無いように見えてしまいます。
実際には`Blocked`のまま3本とも止まったままなのに、`Database::detect_deadlock`はデッドロックを検出できません。

この依存を辺として表すために、待ち行列を先頭から見て隣り合う要求ごとに、後続から手前への辺を追加します。
これは「モードが衝突する保持者」への辺とは別の、待ち行列の位置そのものによる依存です。

`Database`側は、この辺からトランザクションIDごとの隣接表を組み立て、要求元(`owner`)を起点にDFSで自分自身へ戻ってくる経路を探します。
`src/database.rs`に次の`detect_deadlock`を書きます。

```rust
fn detect_deadlock(&mut self, owner: TransactionId) -> DbResult<Option<TransactionId>> {
    let edges = self.lock_manager.wait_for_edges();
    let mut adjacency: HashMap<TransactionId, Vec<TransactionId>> = HashMap::new();
    for (waiter, holder) in edges {
        adjacency.entry(waiter).or_default().push(holder);
    }
    for neighbors in adjacency.values_mut() {
        neighbors.sort_by_key(|t| t.0);
        neighbors.dedup();
    }

    let Some(cycle) = find_cycle_containing(&adjacency, owner) else {
        return Ok(None);
    };
    let victim = cycle.into_iter().max_by_key(|t| t.0).expect("循環は少なくとも1つの要素を持つ");
    self.abort_transaction(victim)?;
    Ok(Some(victim))
}
```

`find_cycle_containing`は`owner`から辺をたどり、`owner`自身に戻ってくる経路を1つ見つけたら、その経路(循環を構成するノードの列)を返します。

```rust
fn dfs(
    node: TransactionId,
    start: TransactionId,
    adjacency: &HashMap<TransactionId, Vec<TransactionId>>,
    path: &mut Vec<TransactionId>,
    on_path: &mut std::collections::HashSet<TransactionId>,
) -> Option<Vec<TransactionId>> {
    let neighbors = adjacency.get(&node)?;
    for &next in neighbors {
        if next == start {
            return Some(path.clone());
        }
        if on_path.contains(&next) {
            continue;
        }
        on_path.insert(next);
        path.push(next);
        if let Some(cycle) = dfs(next, start, adjacency, path, on_path) {
            return Some(cycle);
        }
        path.pop();
        on_path.remove(&next);
    }
    None
}
```

この実装が検出するのは`owner`を含む循環だけです。
`owner`を経由しない、無関係などうしが待ち合っている循環まで探しには行きません。
検出のタイミングを「ロックの要求がBlockedになったその場」に絞ったこの章の設計では、`owner`自身が新しく作った辺こそが調べたい対象であり、それ以外の循環は、それが実際に閉じた瞬間(その循環の中の**どれか**が新しい要求を出してBlockedになった瞬間)に、その要求元を起点として別途検出されます。
周期的にグラフ全体を走査する設計(たとえば一定間隔ですべてのトランザクションを起点に調べ直す)も選べましたが、単一スレッドの決定的インターリーブハーネスでは「いつ調べるか」を時間ではなくテストコードの操作順序で表現する必要があり、「Blockedになった要求ごとに、その場で調べる」という規則が最もテストと対応づけやすい選択でした。

### Victim Selection: 最若TxIDを選ぶ

循環が見つかったら、その中のどれか1本を切らなければ先へ進めません。
切る対象(**Victim**)をどう選ぶかが**Victim Selection**です。
この章は、循環の中で最も新しい(`TransactionId`が最大の)トランザクションを選びます。

```rust
let victim = cycle.into_iter().max_by_key(|t| t.0).expect("循環は少なくとも1つの要素を持つ");
```

`TransactionId`は`BEGIN`のたびに単調増加で採番されるため、値が大きいほど後から始まったトランザクションです。
後から始まったトランザクションほど、それまでに行った作業(書き込み、獲得したロック)が少ない可能性が高く、Abortしたときに捨てる作業量も小さく済みます。
もちろんこれは正確な見積もりではありません(後から始まって大量の書き込みをすでに終えているトランザクションもありえます)が、実際に費やした作業量を計測する仕組みをこのクレートは持たないため、`TransactionId`という手元にある情報だけで決定的に選べる規則として、この単純な指標を選びました。

Victimは`abort_transaction`で即座に強制Abortされます。

```rust
fn abort_transaction(&mut self, victim: TransactionId) -> DbResult<()> {
    let undo_log = if let Some(tx) = &mut self.tx
        && tx.id == victim
    {
        std::mem::take(&mut tx.undo_log)
    } else if let Some(ctx) = self.harness_contexts.get_mut(&victim) {
        std::mem::take(&mut ctx.undo_log)
    } else {
        return Ok(());
    };

    match &mut self.backend {
        Backend::Memory { storage, .. } => transaction::apply_undo_memory(storage, undo_log),
        Backend::Disk { storage } => transaction::apply_undo_disk(storage, undo_log)?,
    }
    self.lock_manager.release_all(victim);

    if let Some(tx) = &mut self.tx
        && tx.id == victim
    {
        tx.state = TransactionState::Aborted;
        tx.victim_of_deadlock = true;
    } else if let Some(ctx) = self.harness_contexts.get_mut(&victim) {
        ctx.state = TransactionState::Aborted;
        ctx.victim_of_deadlock = true;
    }
    Ok(())
}
```

やっていることは`ROLLBACK`(`execute_rollback`)とほぼ同じです。
積んだ`undo_log`を逆順に適用し(第30章の`apply_undo_memory`と`apply_undo_disk`)、ロックをすべて手放します(`release_all`)。
この`release_all`が呼ばれた瞬間、循環はもう存在しません。
Victimが手放したロックの待ち行列は`promote_waiters`によって即座に再評価され、循環の中で次に並んでいた要求(あるいは循環とは無関係にたまたま同じロックを待っていた要求)がそのまま昇格することもあります。

`TransactionContext`自体は、`self.tx`や`harness_contexts`のスロットからは取り除きません。
`state`を`Aborted`に、新しく追加した`victim_of_deadlock`を`true`にするだけです。
`src/transaction.rs`に定義された`TransactionContext`へ、この`victim_of_deadlock`フィールドを追加します。

```rust
pub(crate) struct TransactionContext {
    pub id: TransactionId,
    pub state: TransactionState,
    pub undo_log: Vec<UndoRecord>,
    pub isolation_level: IsolationLevel,
    /// `true`なら、この`state`が`Aborted`になった理由はデッドロック検出の
    /// Victim Selectionである。`false`なら、Statement Error時のAbort
    /// (第30章)、または明示的な`ROLLBACK`によるものである。
    pub victim_of_deadlock: bool,
}
```

このフラグが、呼び出し側に返すエラーを選び分けます。
`src/database.rs`に次の`aborted_error`を追加します。

```rust
fn aborted_error(victim_of_deadlock: bool) -> DbError {
    if victim_of_deadlock { DbError::DeadlockDetected } else { DbError::TransactionAborted }
}
```

`execute_in_tx`、`commit_tx`、`execute`、`execute_commit`が`Aborted`状態のトランザクションへの操作を拒む箇所は、すべてこの関数を経由するように書き換えました。
Victimになったトランザクションへ以後触れるたびに`DbError::DeadlockDetected`が返り、Statement Error時のAbort(第30章)や明示的な`ROLLBACK`後の`DbError::TransactionAborted`とは文言で区別できます。

### 要求元自身がVictimになる場合、ならない場合

ロックの獲得を試みる箇所(`acquire_scan_locks`、`acquire_write_locks`、`Serializable`のINSERT)は、すべて次の1個の関数を経由します。

```rust
fn acquire_lock_or_detect_deadlock(&mut self, owner: TransactionId, key: LockKey, mode: LockMode) -> DbResult<()> {
    if self.lock_manager.acquire(owner, key, mode) == LockResult::Granted {
        return Ok(());
    }
    match self.detect_deadlock(owner)? {
        None => Err(DbError::WouldBlock),
        Some(victim) if victim == owner => Err(DbError::DeadlockDetected),
        Some(_) => {
            // 別のトランザクションをVictimとして倒したことで、`owner`が
            // 待ち行列の中ですでに昇格しているかもしれない
            // (`LockManager::release_all`の`promote_waiters`を参照)。
            if self.lock_manager.acquire(owner, key, mode) == LockResult::Granted {
                Ok(())
            } else {
                Err(DbError::WouldBlock)
            }
        }
    }
}
```

`acquire`が`Blocked`を返したとき、`detect_deadlock`の結果は3通りに分かれます。
循環が見つからなければ、これまでどおりの`WouldBlock`です。
循環が見つかり、Victimが要求元自身(`owner`)であれば、この関数の呼び出し元(`SELECT`や`UPDATE`)は自分自身が今まさにAbortされたことを`DeadlockDetected`として直接受け取ります。

Victimが要求元とは**別のトランザクション**であることもあります。
循環に含まれる複数のトランザクションのうち、たまたま最も新しいのが要求元でない場合です。
この場合、要求元の要求そのものは循環を解消したことで通ることがあります。
Victimが手放したロックを要求元が待ち行列の中で引き継いでいれば、2回目の`acquire`がその場で`Granted`を返すからです。
要求元は、自分の要求がデッドロック解決に巻き込まれたことにすら気づかず、ただ`Ok`を受け取って処理を続けます。
Victimにされた側は、次に自分のトランザクションへ触れたとき(次の文の実行、または`COMMIT`)に初めて`DeadlockDetected`を受け取ります。
`tests/deadlock.rs`に書いた次のテストが、それを確認します。

```rust
#[test]
fn a_different_transaction_can_be_chosen_as_the_victim() {
    // ...
    // T2がtaを欲しがるが、まだ循環は無い(T1は誰も待っていない)。
    assert!(matches!(db.execute_in_tx(&t2, "UPDATE ta SET v = 20 WHERE id = 1"), Err(DbError::WouldBlock)));

    // T1がtbを欲しがると循環が閉じる。要求元はT1だが、循環内で最も新しい
    // T2がVictimに選ばれるため、T1の要求はそのまま成功する。
    db.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1").unwrap();
    db.commit_tx(t1).unwrap();

    // T2は自分がVictimになったことをまだ知らない。次に触れた操作
    // (ここではcommit_tx)で初めてDeadlockDetectedを受け取る。
    assert!(matches!(db.commit_tx(t2), Err(DbError::DeadlockDetected)));
}
```

3本以上のトランザクションが環状に待ち合う場合も、同じ仕組みでそのまま検出できます。
DFSは辺をたどって`owner`へ戻る経路を探すだけなので、循環の長さは2に限定されません。

```rust
#[test]
fn a_three_way_cycle_is_also_detected() {
    // T1はT2の`tb`を、T2はT3の`tc`を、T3はT1の`ta`を待つ。
    // ...
    // T3がta(T1が保持)を欲しがると、T1→T2→T3→T1の循環が閉じる。要求元の
    // T3が循環内で最も新しいトランザクションなので、VictimはT3自身になる。
    assert!(matches!(db.execute_in_tx(&t3, "UPDATE ta SET v = 30 WHERE id = 1"), Err(DbError::DeadlockDetected)));

    // T3が手放したtcを、待っていたT2が引き継ぐ。
    db.execute_in_tx(&t2, "UPDATE tc SET v = 20 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();

    // T2がコミットしてtbを手放したので、待っていたT1も引き継いで完走する。
    db.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1").unwrap();
    db.commit_tx(t1).unwrap();
}
```

T3がVictimとしてAbortされたあとも、T1とT2は1本ずつロックを引き継ぎながら、それぞれ`COMMIT`まで完走します。
デッドロックの解決は、循環に含まれていたトランザクションを1本諦めさせるだけで済み、残りは(多少待たされることはあっても)最終的に必ず前へ進めます。

```console
$ cargo test --test deadlock
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
$ cargo test --lib lock_manager
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## この章の限界

`Serializable`のテーブルロックへの昇格は、`Backend::Disk`の`SELECT`と`INSERT`にしか効きません。
`UPDATE`と`DELETE`はどの分離レベルでもTuple Lockのままです(`acquire_write_locks`は`isolation_level_of`を一切見ません)。
これは意図した設計であり、書き込み側のロックの規律は分離レベルに関係なく常にStrict 2PLだからですが、副作用として、`Serializable`のもとでも異なる行を対象にする2本の`UPDATE`は互いにブロックし合いません。
これは正しさを損ないません(既存の行同士の競合はTuple Lockがすでに検査しています)が、テーブルロックへの昇格が及ぶ範囲が「まだ存在しない行への書き込み」1点に絞られていることは覚えておく必要があります。

Wait-for Graphの構築は、`LockManager::wait_for_edges`をロックの要求が`Blocked`になるたびに毎回`O(待ち行列の総数)`で組み立て直します。
本物のデータベースは、この辺の集合を差分更新するか、検出そのものを周期的なバックグラウンドタスクに任せることでこのコストを分散させます。
このクレートは単一スレッドの決定的インターリーブハーネス上で動くため、バックグラウンドタスクという概念自体がまだ無く(実スレッドが解禁されるのは第35章です)、毎回組み立て直す以外の選択肢がありませんでした。

`find_cycle_containing`は要求元(`owner`)を含む循環だけを検出します。
これ自体は「Blockedになった要求ごとにその場で調べる」という設計のもとでは正しく機能しますが、`owner`を経由しない無関係などうしの循環が同じ瞬間にすでに存在していても、この呼び出しでは見つかりません。
その循環は、それを構成するトランザクションのどれかが次に新しい要求を出したとき、初めて検出されます。
すべてのトランザクションが待ち行列に入ったまま誰も新しい要求を出さない、という状況(この章のテストでは作れませんでしたが、理論上は考えられます)では、循環が存在し続けたまま検出されない時間が生じます。

## 演習問題

### 必須課題

1. `acquire_scan_locks`の`ReadCommitted`分岐は、`SELECT`の対象になった鍵をすべて`release_keys`で即座に手放します。
この`SELECT`が複数のテーブルを走査するとき(`JOIN`など)、一部のテーブルのロックだけ手放し忘れる、あるいは余計に手放してしまうバグが起きうる箇所はどこか、`acquire_scan_locks`と`collect_scan_tables`(第31章)を読んで考えてください。
2. `abort_transaction`は、Victimの`TransactionContext`を`self.tx`または`harness_contexts`のどちらかから見つけますが、通常のSQL経路(`self.tx`)とハーネス経路(`begin_tx`)を同時に使った場合、Victimの`TransactionContext`がどちらにも見つからない状況が起こりうるかどうかを考えてください(ヒント: `execute_in_tx`が`self.tx`を一時的に差し替える箇所を読んでください)。
3. `find_cycle_containing`は決定的な結果を返すために、`detect_deadlock`が事前に隣接リストを`TransactionId`の昇順にソートしています。
このソートを取り除くとテストの結果がどう変わるか(あるいは変わらないか)、`HashMap`の反復順序に依存する形で実際に確認してください。

### 発展課題

1. `Serializable`のテーブルロックへの昇格は、Phantomと無関係な範囲への`INSERT`まで一律にブロックします。
本文で触れたKey Range Lockを設計し、`WHERE balance > 40`のような単純な比較述語1つに対してだけ範囲ロックを実装してみてください。
索引(第23章と第24章のB+Tree)のキー順序をどう使えば、範囲ロックの対象を絞り込めるか考えてください。
2. Victim Selectionの方針を「最若TxID」から「保持しているロックの数が最も少ないトランザクション」に変えてみてください。
`cycle`の各要素について、その時点で保持しているロックの数を`LockManager`から取得する方法を設計し、実装したうえで`tests/deadlock.rs`の3本のテストがどう振る舞うか確認してください。
3. この章はWait-for Graphの検出をロックの要求が`Blocked`になった瞬間に限定しました。
一定回数の`execute_in_tx`呼び出しごとに、全トランザクションを起点とした検出を走らせる「周期的な検出」を実装し、要求元を経由しない循環(本章の「この章の限界」で触れた、無関係などうしの循環)も検出できるようにしてください。
