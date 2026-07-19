# 第31章 Lock ManagerとStrict 2PL

同じ`accounts`テーブルの`id = 1`の行を、2本のトランザクションが同時に触るとどうなるでしょうか。
第30章の決定的インターリーブテストハーネスを使って、次の順序で交互実行してみます。

```text
T1: BEGIN
T1: SELECT balance FROM accounts WHERE id = 1        -- 100
T2: BEGIN
T2: SELECT balance FROM accounts WHERE id = 1        -- 100
T2: UPDATE accounts SET balance = 120 WHERE id = 1
T2: COMMIT
T1: UPDATE accounts SET balance = 110 WHERE id = 1
T1: COMMIT
SELECT balance FROM accounts WHERE id = 1             -- 110
```

T1もT2も、最初に読んだ100を根拠に自分の新しい残高を計算しています。
T2が先に120でコミットしますが、T1はそのコミットを知らないまま、自分が最初に読んだ100を根拠にした110を書いてコミットします。
結果として残高は110になり、T2が足した20はどこにも残りません。
第30章はこの現象に**Lost Update**という名前を与え、それが実際に起きることをテストとして固定しました。

第30章の`Database`には、これを止める手段が1つもありませんでした。
`UPDATE`は実行された瞬間に`backend`(`MemStorage`または`Storage`)へ反映され、他のトランザクションはその変更を`COMMIT`より前から読めます。
`ROLLBACK`のための`undo_log`はありましたが、それは「あとで取り消す」ための記録であって、「今は待たせる」ための仕組みではありません。
2本のトランザクションが同じ行に触れようとしたとき、どちらか一方を待たせる仕組みがなければ、Lost Updateのような上書きは防げません。

この章はその「待たせる」仕組みを実装します。
**Lock Manager**が、行やテーブルごとに「今どのトランザクションがどんなロックを持っているか」を記録し、両立しないロックの要求を突き返します。
突き返された要求がどうなるのか、突き返された側は具体的に何を待つのか、そして待たせるという操作がAtomicity(第30章)とどう両立するのかを、この章を通して詰めていきます。

## Shared/Exclusiveロックと互換性行列

ロックには2種類のモードがあります。
読み取りのための**Shared**ロックと、書き込みのための**Exclusive**ロックです。

```rust
pub enum LockMode {
    /// 読み取り用。複数のトランザクションが同じ対象に同時に持てる。
    Shared,
    /// 書き込み用。1つのトランザクションしか同時に持てない。
    Exclusive,
}
```

2つのロックが同じ対象に同時に存在してよいかどうかは、次の**互換性行列**で決まります。

| 保持中\要求 | Shared | Exclusive |
|---|---|---|
| Shared | 両立する | 両立しない |
| Exclusive | 両立しない | 両立しない |

Shared同士だけが両立します。
複数のトランザクションが同じ行を同時に読むことは何の問題も起こしませんが、誰かが書いている間に別の誰かが読む、あるいは誰かが書いている間に別の誰かも書く、という組み合わせはどちらも許されません。
先ほどのLost Updateの例で言えば、T1とT2がどちらも`UPDATE`する前にExclusiveロックを取ろうとしていれば、後から来た側は先に来た側が終わるまで待たされていたはずです。

ロックが何に対するものかを表す型が`LockKey`です。

```rust
pub enum LockKey {
    /// テーブル全体。
    Table(TableId),
    /// 1行(`RecordId`はDiskバックエンドだけが持つ、第30章の`transaction`
    /// モジュールを参照)。
    Tuple(TableId, RecordId),
}
```

`Table`はテーブル全体を1個の対象として扱う粗い粒度、`Tuple`は1行だけを対象にする細かい粒度です。
docs-local(第4部までの構成メモ)の原案は「最初はTable Lockで正しさを確立し、その後Tuple Lockへ細粒度化する順序が安全」と述べています。
この章もその順序を踏みます。
Lock Managerの中身をまず`TableId`だけで検証し、それから`Database`へ組み込む段になって`LockKey`(TableとTupleの両方を表せる型)へ差し替えます。
ロックの管理ロジック自体は、この差し替えの前後で1文字も変わりません。
どちらの粒度を実際に使うかは`Database`のバックエンド(Memory、Disk)によって決まり、その理由は本章の後半「SELECT/DMLへの組み込みとロックの粒度」で扱います。

## Wait Queue: 待たせて、あとで再評価する

ロックが取れなかったとき、要求を出したトランザクションはどうなるべきでしょうか。
本物のデータベースは、ロックを取れなかったスレッドを実際にブロックします。
`Mutex::lock()`が返ってこないのと同じ意味で、OSのスケジューラがそのスレッドを止め、ロックが解放されたときに起こします。

このクレートにはその手段がありません。
第30章が導入した決定的インターリーブテストハーネスは、複数のトランザクションを実スレッドではなく単一スレッド上の交互実行で再現しています。
Buffer PoolとB+Treeがスレッドセーフになるのは第35章であり、それより前にスレッドを増やす選択肢はありません。
そこでこの章の`LockManager::acquire`は、ロックを取れないときブロックする代わりに、`LockResult::Blocked`という**値**を返してすぐに制御を戻します。

```rust
pub enum LockResult {
    /// ロックを獲得できた。呼び出し元はそのまま処理を続けてよい。
    Granted,
    /// 他のトランザクションが両立しないロックを持っているため、獲得できな
    /// かった。要求は待ち行列の末尾(Lock Upgradeの場合は先頭、本文を参照)に
    /// 積まれており、対象のロックが解放されるたびに[`LockManager::release_all`]
    /// が再評価する。呼び出し元は今すぐこの要求を諦めるのではなく、あとで
    /// もう一度同じ要求を試すことを想定している(モジュール冒頭の説明を参照)。
    Blocked,
}
```

`Blocked`を受け取った要求は消えません。
`LockManager`の内部で、対象ごとの**Wait Queue**(待ち行列)に積まれたままになります。

```rust
struct Waiter {
    txn: TransactionId,
    mode: LockMode,
    /// この要求が、すでに`Shared`を持っているトランザクションによる
    /// `Exclusive`へのUpgrade要求かどうか。Upgrade要求は待ち行列の並び方が
    /// 通常の新規要求と違う(`LockManager::acquire`のドキュメントを参照)。
    is_upgrade: bool,
}
```

1つの対象(`LockKey`1個)は、今それを持っているトランザクションの集合(`holders`)と、取れずに並んでいるトランザクションの列(`waiters`)を持ちます。

```rust
struct LockEntry {
    holders: Vec<(TransactionId, LockMode)>,
    waiters: VecDeque<Waiter>,
}
```

ロックが解放されるたび、`release_all`が待ち行列の先頭から順に昇格できるかどうかを再評価します。

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
}
```

再評価の中身が`promote_waiters`です。

```rust
fn promote_waiters(&mut self, key: &K) {
    let Some(entry) = self.entries.get_mut(key) else { return };
    while let Some((is_upgrade, txn, mode)) = entry.waiters.front().map(|w| (w.is_upgrade, w.txn, w.mode)) {
        if is_upgrade {
            let is_sole_holder = entry.holders.len() == 1 && entry.holders[0].0 == txn;
            if !is_sole_holder {
                break;
            }
            entry.waiters.pop_front();
            entry.holders[0].1 = LockMode::Exclusive;
            continue;
        }
        if !entry.compatible_with_holders(mode) {
            break;
        }
        entry.waiters.pop_front();
        entry.holders.push((txn, mode));
    }
    if entry.holders.is_empty() && entry.waiters.is_empty() {
        self.entries.remove(key);
    }
}
```

待ち行列の先頭がShared要求で、それが今の保持者集合と両立すれば昇格させ、次の要求に進みます。
Shared同士は何人いても両立するため、先頭に連続して並んだShared要求はまとめて昇格します。
ところが先頭がExclusive要求(または成立しないUpgrade要求)だった場合はそこで止まります。
2番目以降に、今なら通るはずのShared要求が控えていても、先へは進みません。

ここが**公平性**の要です。
先頭のExclusive要求を飛び越して後続のShared要求を先に通してしまうと、Exclusive要求は後から来るShared要求に際限なく追い越され続け、いつまでもロックを取れなくなります。
これがStarvation(飢餓)です。
`promote_waiters`が「先頭が両立しなければ即座に止める」という単純な規則を守っているのは、この追い越しを起こさないためです。
新規要求を積む側にも同じ規則が働きます。
`acquire`は、待ち行列が空でない限り、たとえ今の保持者と両立するShared要求であっても即座には許可せず、末尾に並ばせます。

```rust
if entry.waiters.is_empty() && entry.compatible_with_holders(mode) {
    entry.holders.push((txn, mode));
    return LockResult::Granted;
}
```

この2つの規則(先頭からしか昇格させない、待ち行列が空でなければ新規要求も並ばせる)が揃って、この`LockManager`はFIFOに沿った公平性を持ちます。
テストでこの振る舞いを確認しています。

```rust
#[test]
fn a_later_shared_request_does_not_overtake_an_earlier_exclusive_request() {
    let mut lm = LockManager::new();
    assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
    // T2のExclusiveはT1のSharedとぶつかりBlocked。
    assert_eq!(lm.acquire(T2, table(1), LockMode::Exclusive), LockResult::Blocked);
    // T3の新規SharedはT1とは両立するはずだが、待ち行列の先頭(T2)を
    // 追い越せないため、やはりBlockedになる。
    assert_eq!(lm.acquire(T3, table(1), LockMode::Shared), LockResult::Blocked);
    assert_eq!(lm.waiting_order(&table(1)), vec![T2, T3]);
    // ...
}
```

## Lock Upgrade

トランザクションが自分の読んだ行を書き換えたくなることはよくあります。
`SELECT`でSharedロックを取った行を、続く`UPDATE`でExclusiveへ切り替えたい場合です。
これを新しい要求として扱うと、自分自身のSharedロックが自分自身の新しい要求とぶつかってしまい、常にBlockedになってしまいます。
そこで`acquire`は、要求元がすでに同じ対象へ何らかのロックを持っている場合を別扱いします。

```rust
if let Some(held) = entry.holder_mode(txn) {
    if held == LockMode::Exclusive || held == mode {
        return LockResult::Granted;
    }
    // held == Shared, mode == Exclusive: Lock Upgrade。
    if entry.holders.len() == 1 {
        entry.holders[0].1 = LockMode::Exclusive;
        return LockResult::Granted;
    }
    if !entry.waiters.iter().any(|w| w.txn == txn) {
        entry.waiters.push_front(Waiter { txn, mode, is_upgrade: true });
    }
    return LockResult::Blocked;
}
```

すでに持っているロックが要求と同じか、それより強ければ(Exclusiveは何でも満たす)そのまま`Granted`です。
Sharedを持っていてExclusiveを要求した場合が**Lock Upgrade**であり、要求元がそのキーの唯一の保持者であれば、他の誰とも衝突しないためその場でモードを書き換えて`Granted`にします。
他にもSharedを持つトランザクションがいれば、Upgrade要求は待ち行列の**先頭**に積まれます。

末尾ではなく先頭に積むところが、新規要求との違いです。
Upgradeを要求するトランザクションは、無関係な割り込みではありません。
すでに部分的な権利(Shared)をそのキーに対して持っている、既存の参加者です。
これを末尾に積んでしまうと、あとから来た無関係な新規Shared要求に何度も追い越され、Upgradeだけがいつまでも成立しない**Upgrade starvation**が起こります。
先頭に積むことで、Upgrade要求は「これから新しく来る要求」より先に扱われ、あとは他の保持者が抜けるのを待つだけの状態になります。

```rust
#[test]
fn upgrade_request_is_inserted_ahead_of_new_shared_requests() {
    let mut lm = LockManager::new();
    assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
    assert_eq!(lm.acquire(T2, table(1), LockMode::Shared), LockResult::Granted);
    // T1がUpgradeをブロックされたまま待つ。
    assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Blocked);
    // 後から来たT3の新規Shared要求は、末尾に積まれる。
    assert_eq!(lm.acquire(T3, table(1), LockMode::Shared), LockResult::Blocked);
    assert_eq!(lm.waiting_order(&table(1)), vec![T1, T3], "UpgradeのT1が新規要求のT3より先頭にいる");
}
```

## Strict 2PL: Growingだけで、Shrinkingを独立に持たない理由

ロックをいつ取るかだけでなく、いつ手放すかも正しさに直結します。
**Two-Phase Locking**(2PL)は、トランザクションの実行を2つの局面に分ける規律です。
ロックを取るだけの**Growing Phase**と、ロックを手放すだけの**Shrinking Phase**であり、1度でも手放したら二度と新しいロックを取ってはいけません。

この規律だけでは、まだ足りません。
Growing PhaseとShrinking Phaseの境目を実行の途中に置くと、境目より後ろでは新しいロックを取れないのに、境目より前に取ったロックはまだ手放されていない、という中途半端な区間が生まれます。
その区間の間に、別のトランザクションが「もう手放されたロック」を取って書き込みを始めてしまうと、まだコミットしていない側の変更が外から見えてしまいます。
**Strict 2PL**は、Shrinking Phaseの開始を「トランザクションが確定するとき」1点に固定することでこれを防ぎます。
言い換えると、Growing Phaseの間は取得だけを行い、`COMMIT`または`ROLLBACK`が届いた瞬間に、それまで取ったロックをまとめて手放します。
独立したShrinking Phaseを持たないのは省略ではなく、Strict 2PLという規律そのものの定義です。

このクレートでの実装は素直です。
`execute_bound_statement`が、文を実行する前にこの文のロック保持者(`owner`)を決めます。

```rust
fn execute_bound_statement(&mut self, statement: Statement, sql: &str) -> DbResult<QueryResult> {
    let bound = self.bind(statement, sql)?;
    let owner = self.lock_owner();
    let result = match bound {
        BoundStatement::Select(select) => self.execute_select(logical_plan::build_select(*select), owner),
        // ...(CreateTable・DropTable等は変わらず)
        BoundStatement::Update(update) => self.execute_update(logical_plan::build_update(update), owner),
        BoundStatement::Delete(delete) => self.execute_delete(logical_plan::build_delete(delete), owner),
        // ...
    };
    if self.tx.is_none() {
        self.lock_manager.release_all(owner);
    }
    result
}
```

`owner`は、`Active`なトランザクションがあればその`TransactionId`、無ければ(Autocommit)この1文だけのために新しく割り当てたIDです。

```rust
fn lock_owner(&mut self) -> TransactionId {
    match &self.tx {
        Some(tx) => tx.id,
        None => {
            let id = TransactionId(self.next_txn_id);
            self.next_txn_id += 1;
            id
        }
    }
}
```

`self.tx`が`None`(Autocommit)のときだけ、文の実行が終わった直後に`release_all`を呼びます。
Autocommitの1文はそれ自体が完結したトランザクションであり、Strict 2PLで言うところの「確定するとき」は文の終わりそのものです。
`Active`なトランザクションの中であれば、`self.tx`は`Some`のままなのでここでは手放さず、`COMMIT`、`ROLLBACK`まで持ち越します。

```rust
fn execute_commit(&mut self, _commit: CommitStatement) -> DbResult<QueryResult> {
    match &self.tx {
        None => Err(DbError::NoActiveTransaction),
        Some(tx) if tx.state == TransactionState::Aborted => Err(DbError::TransactionAborted),
        Some(_) => {
            let tx = self.tx.take().expect("直前のmatchでSomeを確認済み");
            self.lock_manager.release_all(tx.id);
            Ok(QueryResult::command("COMMIT"))
        }
    }
}
```

`execute_rollback`も同様に、`undo_log`を逆順適用したあとで`release_all`を呼びます。
ハーネス専用の`commit_tx`、`rollback_tx`も、SQL経路の`execute_commit`、`execute_rollback`とまったく同じ理由で`release_all`を呼びます。

もう1つ、`WouldBlock`(ロックを獲得できなかったという結果)は、他のエラーとは扱いを変える必要があります。
第30章の`finish`は、`Active`なトランザクション中に実行した文が失敗したら状態を`Aborted`へ倒していました。

```rust
fn finish(&mut self, result: DbResult<QueryResult>) -> DbResult<QueryResult> {
    if matches!(result, Err(DbError::WouldBlock)) {
        return result;
    }
    if result.is_err()
        && let Some(tx) = &mut self.tx
        && tx.state == TransactionState::Active
    {
        tx.state = TransactionState::Aborted;
    }
    result
}
```

`WouldBlock`をこの遷移から除外しているのは、ロックが取れなかった文は一切実行されていないからです。
書き込みも起きておらず、`undo_log`への記録もありません。
この文を「失敗」として扱い`Aborted`へ倒してしまうと、トランザクションは`ROLLBACK`しか受け付けなくなり、ロックが空くのを待ってから同じ文をもう一度試すという選択肢そのものが失われます。
`WouldBlock`は失敗ではなく、「まだ順番が来ていない」という一時的な状態です。

## SELECT/DMLへの組み込みとロックの粒度

ここまでの`LockManager`は`Database`から独立した部品でした。
実際に`SELECT`、`INSERT`、`UPDATE`、`DELETE`がどんなロックを取るかを決めるのが、この節です。

| 文 | Memoryバックエンド | Diskバックエンド |
|---|---|---|
| `SELECT` | テーブル全体にShared | その時点で存在する行全部にShared |
| `INSERT` | テーブル全体にExclusive | ロックしない |
| `UPDATE` | テーブル全体にExclusive | `WHERE`に一致した行だけにExclusive |
| `DELETE` | テーブル全体にExclusive | `WHERE`に一致した行だけにExclusive |

Memoryバックエンド(`MemStorage`)は`RecordId`という概念を持ちません。
行は`Vec<Tuple>`の並びでしかなく(第30章の`transaction`モジュールの説明のとおり)、1行だけを指す安定した識別子がないため、`LockKey::Table`だけを使います。
`INSERT`もテーブル全体のExclusiveを取るため、他のトランザクションが同じテーブルにSharedを持っている間は新しい行を差し込めません。

Diskバックエンド(`Storage`)は`RecordId`を持つため、`LockKey::Tuple(table_id, rid)`という、より細かい粒度に切り替えます。
`SELECT`はその時点でテーブルに**存在する**行の`RecordId`をすべて列挙し、それぞれにSharedを掛けます。

```rust
fn acquire_scan_locks(&mut self, owner: TransactionId, table_ids: &[TableId], mode: LockMode) -> DbResult<()> {
    match &self.backend {
        Backend::Memory { .. } => {
            for &table_id in table_ids {
                if self.lock_manager.acquire(owner, LockKey::Table(table_id), mode) == LockResult::Blocked {
                    return Err(DbError::WouldBlock);
                }
            }
        }
        Backend::Disk { storage } => {
            for &table_id in table_ids {
                let rids: Vec<RecordId> =
                    storage.scan(table_id)?.map(|entry| entry.map(|(rid, _)| rid)).collect::<DbResult<_>>()?;
                for rid in rids {
                    let key = LockKey::Tuple(table_id, rid);
                    if self.lock_manager.acquire(owner, key, mode) == LockResult::Blocked {
                        return Err(DbError::WouldBlock);
                    }
                }
            }
        }
    }
    Ok(())
}
```

`WHERE`で絞り込む前に、テーブルに存在する行すべてをロックの対象にしているのは単純化です。
`SELECT`は列も`WHERE`も多様な形を取り、`Filter`が複数重なったり`JOIN`をまたいだりするため、`LogicalPlan`の木から「結局どの行を読むか」を一般には特定できません。
この章はその特定を見送り、絞り込み前の全行を対象にする代わりに、`WHERE`の評価を二重に行わずに済ませました。

書き込み側(`UPDATE`、`DELETE`)は事情が違います。
対象はただ1個の`table_id`と`predicate`に決まるため、`WHERE`に一致した行だけを先に確定させてからロックできます。

```rust
fn acquire_write_locks(
    &mut self,
    owner: TransactionId,
    table_id: TableId,
    schema: &Schema,
    predicate: Option<&BoundExpr>,
) -> DbResult<()> {
    match &self.backend {
        Backend::Memory { .. } => {
            if self.lock_manager.acquire(owner, LockKey::Table(table_id), LockMode::Exclusive) == LockResult::Blocked
            {
                return Err(DbError::WouldBlock);
            }
        }
        Backend::Disk { storage } => {
            let rids = executor::storage_matching_rids(storage, table_id, schema, &self.functions, predicate)?;
            for rid in rids {
                let key = LockKey::Tuple(table_id, rid);
                if self.lock_manager.acquire(owner, key, LockMode::Exclusive) == LockResult::Blocked {
                    return Err(DbError::WouldBlock);
                }
            }
        }
    }
    Ok(())
}
```

`storage_matching_rids`は、`storage_update`、`storage_delete`の冒頭にある走査とまったく同じ絞り込みをもう一度行い、対象の`RecordId`だけを返す関数です。
`WHERE`を二重に評価することにはなりますが、ロックの獲得と実際の書き込みを1回の走査に統合する配線はこの章の範囲を超えるため見送りました。
この絞り込みの見返りとして、`id`の異なる行を書き換える2本の`UPDATE`は、Diskバックエンドでは互いにブロックし合いません。

```rust
#[test]
fn different_rows_do_not_block_each_other_under_tuple_lock() {
    let (mut db, path) = accounts_db("interleave-disk-different-rows");
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    // t2はt1と別の行(id=2)だけを書き換えるので、ブロックされずに成功する。
    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 55 WHERE id = 2").unwrap();

    db.commit_tx(t1).unwrap();
    db.commit_tx(t2).unwrap();
    // ...
}
```

Memoryバックエンドで同じ状況を試すと、`accounts`テーブル全体のExclusiveが衝突し、2本目の`UPDATE`はブロックされます。
これがTable LockとTuple Lockの実質的な違いであり、原案が「Tuple Lockへ細粒度化する」と呼んでいたものの中身です。

最後に`INSERT`です。
`Backend::Disk`の`INSERT`は**何もロックしません**。
新しく挿入される行の`RecordId`は、挿入が終わるまで存在しないため、そもそもロックする対象がないのです。
この非対称性、つまり既存の行はTuple Lockで守られるのに、まだ存在しない行は誰も守らないという構造そのものが、Phantomがこの章を通り抜けてもなお生き残る理由です。

## テストで確認する: 反転した異常系とデッドロックの観測

第30章は`tests/interleave.rs`に、Lost Update、Dirty Read、Non-repeatable Read、Phantomという4つの異常が実際に「起きる」ことを`assert`するテストを残しました。
このハーネスの役割は、その反転が実際に起きたことを、同じテストの`assert`を書き換えるだけで確認できるようにしておくことでした。
この章はその約束を果たします。

Memoryバックエンド(`tests/interleave.rs`)では、テーブル単位のロックが4つすべてを防ぎます。

```rust
#[test]
fn dirty_read_is_prevented_by_table_lock() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    // T1は結局ロールバックし、ロックを手放す。
    db.rollback_tx(t1).unwrap();

    // T2が読めるのは、T1がロールバックしたあとの値(100)だけである。
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let value = int_value(&read, 0, 0);
    db.commit_tx(t2).unwrap();

    assert_eq!(value, 100, "T2は一度もコミットされていない値(70)を読めない(Dirty Readが起きない)");
}
```

T2の`SELECT`は、T1がExclusiveロックを持っている間`WouldBlock`を返し、T1がロールバックしてロックを手放すまで一度も実行されません。
再試行して初めて読める値は、T1がロールバックしたあとの100だけです。
Lost Update、Non-repeatable Read、Phantomも同じ形で反転し、Memoryバックエンドの粗い粒度のもとでは4つとも起きなくなります。

Diskバックエンド(`tests/interleave_disk.rs`)では、結果が3対1に分かれます。
Lost Update、Dirty Read、Non-repeatable ReadはTuple Lockでも防がれますが、Phantomだけは防がれません。

```rust
#[test]
fn phantom_read_is_not_prevented_by_tuple_lock() {
    let (mut db, path) = accounts_db("interleave-disk-phantom");
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    assert_eq!(int_value(&first, 0, 0), 2);

    // Table Lock(`tests/interleave.rs`)と違い、T2のINSERTはブロックされない。
    db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)").unwrap();
    db.commit_tx(t2).unwrap();

    let second = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let second_count = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_count, 3, "T2が挿入した行(幻)が2回目の集計に現れている(Phantomはまだ起きる)");
    // ...
}
```

T1が集計のために取ったロックは、その時点で存在するid=1、id=2の行だけを対象にしています。
T2が挿入するid=3の行は、挿入が終わるまでロックする対象自体が存在しないため、T1の集計中でも何にも妨げられずに成功します。
この1本だけが「まだ起きる」テストとして残るのは、偶然ではなく、Tuple Lockという粒度の構造的な限界です。
Phantomをふさぐには、まだ存在しない行の範囲そのものをロックする仕組み(Key Range Lock)か、テーブル全体をロックする粗い粒度に戻るしかなく、そのどちらも第32章の主題です。

この章はデッドロックの**検出**を行いません。
2本のトランザクションが互いの持つロックを欲しがる状況は、この章のLock Managerでも普通に起こります。

```rust
#[test]
fn mutual_wait_leaves_both_transactions_blocked_without_detection() {
    let mut lm = LockManager::new();
    assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
    assert_eq!(lm.acquire(T2, table(2), LockMode::Exclusive), LockResult::Granted);

    // T1はT2の持つtable(2)を、T2はT1の持つtable(1)を欲しがる。
    assert_eq!(lm.acquire(T1, table(2), LockMode::Exclusive), LockResult::Blocked);
    assert_eq!(lm.acquire(T2, table(1), LockMode::Exclusive), LockResult::Blocked);

    // どちらも自動的には解決されない。このLock Manager自身はデッドロック
    // 検出を行わないため、2つとも待ち行列に残ったままである
    // (検出・解決は第32章のWait-for Graph)。
    let blocked = lm.blocked_transactions();
    assert!(blocked.contains(&T1));
    assert!(blocked.contains(&T2));
}
```

T1もT2も、相手が持っているロックを解放するのを待ったまま、待ち行列に残り続けます。
このテストが確認しているのは「デッドロックが起きないこと」ではなく、「デッドロックが起きたとき、この章の仕組みだけでは両者とも動けなくなること」です。
どちらかを見つけ出して強制的に`ROLLBACK`させる**Victim Selection**は、Wait-for Graphによる検出とあわせて第32章で実装します。

```console
$ cargo test --lib
test result: ok. 766 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out
$ cargo test --test interleave
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
$ cargo test --test interleave_disk
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## この章の限界

`EXPLAIN`、`EXPLAIN ANALYZE`は、この章のロックを一切経由しません。
`EXPLAIN`(非`ANALYZE`)は計画を文字列化するだけで実行しないため元々ロックが要りませんが、`EXPLAIN ANALYZE SELECT`は実際に`Executor`を実行するにもかかわらず、`execute_select`とは別に`Executor`を組み立てる経路を通るため、ロックの獲得を差し込んでいません。

`SELECT`のロックは、`WHERE`で絞り込む前にテーブルへ存在する行すべてを対象にしています。
これは正しさを損ないません(対象を広く取りすぎているだけで、狭すぎて見落とすことはありません)が、`UPDATE`、`DELETE`が持つような「関係ない行同士はブロックし合わない」という利点を`SELECT`は得られません。
`WHERE`の評価をロックの獲得へ橋渡しする配線は、演習課題として残します。

`CREATE TABLE`、`DROP TABLE`、`CREATE INDEX`、`DROP INDEX`は、この章のロックの対象外です。
第30章のUndoが同じ理由でDDLを対象外にしていたのと同じ割り切りであり、この章で新たに広げたものではありません。

## 演習問題

### 必須課題

1. `acquire_write_locks`のDisk分岐は、`storage_matching_rids`で対象行を確定させたあと、`storage_update`、`storage_delete`が同じ`predicate`をもう一度評価します。この二重評価を1回にまとめるには、ロックの獲得と実際の書き込みをどう1つの走査に統合すればよいか、設計を考えてください(ヒント: `storage_update`が`planned`を作り終えた直後、実際に書き込む前にロックを獲得する位置を探してください)。
2. `LockManager::acquire`のUpgrade分岐から、待ち行列の先頭ではなく末尾に積む(`push_back`)よう変更するとどうなるか、実際に試してください。`upgrade_request_is_inserted_ahead_of_new_shared_requests`がどう失敗するか観察し、Upgrade Starvationが実際に起こる手順を組み立ててください。
3. `acquire_scan_locks`(`SELECT`が使う)は`WHERE`を無視してテーブルの全行をロックします。`accounts`に1,000行あるとき、`id = 1`だけを読む`SELECT`と`id = 999`だけを更新する`UPDATE`が、この章の実装ではブロックし合うことを確認するテストを書いてください。

### 発展課題

1. `LockManager<K>`は`K`をジェネリクスにしています。`LockKey`の代わりに`TableId`と`RecordId`をそれぞれ別々の`LockManager`(`LockManager<TableId>`、`LockManager<RecordId>`)として持ち、テーブル単位のIntention Lock(IS/IX)を導入する設計を考えてみてください。実際のPostgreSQL、MySQLが採用している多粒度ロック(Multi-Granularity Locking)を調べ、この章の実装との違いを比較してください。
2. `promote_waiters`は、待ち行列の先頭から見て両立する限り昇格させるという規則を持ちます。この規則を「待ち行列全体を見て、両立するものは順序に関係なくすべて昇格させる」という規則に変えると、公平性がどう崩れるか、具体的な待ち行列の例を1つ作って説明してください。
3. デッドロックが実際に起きた状態(`mutual_wait_leaves_both_transactions_blocked_without_detection`と同じ状況)から、`LockManager`に触れずに`Database`側だけの工夫でどちらかのトランザクションを強制的に諦めさせる(`rollback_tx`を呼ぶ)方法はあるか考えてください。第32章のWait-for Graphを実装する前に、この場当たり的な対処にどんな限界があるかを書き出してみてください。
