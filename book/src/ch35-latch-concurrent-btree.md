# 第35章 Latchと並行B+Tree

第30章から第34章まで、`tests/interleave.rs`、`tests/interleave_disk.rs`、`tests/isolation_levels.rs`、`tests/deadlock.rs`が検証してきた「並行実行」は、すべて単一スレッドの上で起きていました。
`Database::begin_tx`で複数のトランザクションを開き、`execute_in_tx`をテストコードが手で交互に呼ぶことで、2つのトランザクションが同時に実行されているかのような状況を組み立てる。
これが決定的インターリーブテストハーネス(第30章)の実体です。

このハーネスには、はっきりした利点がありました。
ステップの順序をテストコード側が完全に握っているので、「T1がロックを取った直後にT2が同じ行を要求する」というような、実スレッドでは狙って再現するのが難しい局面を、1行のコードで固定できます。
Strict 2PL(第31章)やデッドロック検出(第32章)、WALファースト不変条件(第33章)の検証は、この決定性に支えられていました。

一方で、このハーネスが検証していないことも、はっきりしています。
`std::thread::spawn`した本物のOSスレッドが、同じ`BufferPool`の同じフレームへ、同じB+Treeの同じページへ、本当に同時にアクセスしたらどうなるか。
これは第30章から一度も検証されていません。
`BufferPool`(第14章)のフレーム本体は`Mutex`で守られており、2つの`PageReadGuard`が同じページを同時に読むことさえできませんでした。
`BTree`(第23章、第24章)の`insert`は、ページを1枚ずつpinしては手放す設計で、複数階層を同時にpinし続けることもありませんでした。
どちらも「いつか複数スレッドから使われる」ことを見越した`&self`のAPIを持ちながら、実際に複数スレッドから叩かれた場合の正しさは、これまで一度も試されていません。

この章は、その「試されていない」を解消します。
`BufferPool`のフレームを`RwLock`化し、`BTree`にLock Couplingを実装し、`Database`をスレッド間で共有する最小限の器を用意したうえで、実スレッドの統合テストを解禁します。

## Lock(トランザクション間)とLatch(メモリ構造の保護)

ここまでの5部を通して、「ロック」という言葉は1種類しか登場していませんでした。
第31章の`LockManager`が管理する、トランザクション同士の**Lock**です。
`UPDATE`が行の`Exclusive`ロックを取り、別のトランザクションの`SELECT`がそれと衝突して待たされる。
Lockが直列化しているのは、**トランザクションという論理的な単位**同士の実行順序でした。
保持期間はトランザクションの生存期間と同じで、`COMMIT`か`ROLLBACK`まで手放しません(Strict 2PL、第31章)。
デッドロックが起こりうる代わりに、Wait-for Graphによる検出と解決の手段を持っています(第32章)。

この章が導入する**Latch**は、まったく違う対象を守ります。
守るのは、`BufferPool`のフレームや`BTree`のページという、**プロセスのメモリ上のデータ構造**そのものです。
2つのスレッドが同時に同じ`Vec`へ`push`すれば`Vec`の内部状態が壊れるのと同じ理屈で、2つのスレッドが同時に同じページのバイト列を書き換えれば、そのページは壊れます。
Latchはその壊れを防ぐためだけに存在し、トランザクションの論理的な正しさ(Isolation Level、第32章)には関与しません。
保持期間はLockよりずっと短く、そのページに触れている数マイクロ秒だけです。
デッドロックを起こさないよう、取得順序をあらかじめ固定するという設計上の規律で防ぎ、Lock Managerのような検出機構は持ちません。

| | Lock(第31章) | Latch(この章) |
|---|---|---|
| 何を直列化するか | トランザクション同士の実行順序 | メモリ上のデータ構造への同時アクセス |
| 保持期間 | `COMMIT`か`ROLLBACK`まで(Strict 2PL) | そのページに触れている間だけ |
| 対象の単位 | 行やテーブル(`LockKey`) | ページ1枚(`BufferPool`のフレーム) |
| 衝突したときの挙動 | 待つ、または`WouldBlock`を返す | 待つ(すぐ空くはずなので) |
| デッドロックへの対処 | Wait-for Graphで検出し、Victimを強制Abort | 取得順序の規律で構造的に起こさない |
| この教材でのAPI | `crate::lock_manager::LockManager` | `crate::buffer_pool::BufferPool`の`RwLock<Frame>` |

この2つは互いに独立した仕組みであり、片方があるからもう片方が要らなくなるわけではありません。
`UPDATE`は、Lockで「他のトランザクションがこの行を触っていないこと」を保証したうえで、Latchで「この行が乗っているページを今まさに書き換えている最中に、別のスレッドがそのバイト列を読んだり書いたりしないこと」を保証します。
Lockが無ければ、コミット前の変更を他のトランザクションが読んでしまいます(Dirty Read、第30章)。
Latchが無ければ、書き換えている途中の半端なバイト列を誰かが読んでしまいます。
どちらも起こり方は違いますが、どちらも「同時に触れてはいけないものに同時に触れた」結果です。

## Buffer PoolのLatch化: `Mutex<Frame>`から`RwLock<Frame>`へ

第14章の`BufferPool`は、フレーム本体を`Vec<Mutex<Frame>>`で持っていました。
`Mutex`はExclusiveの区別しか持たないため、同じページを読むだけの`PageReadGuard`が2つあっても、片方が生きている間はもう片方の`read_page`がロック待ちで止まります。
これが第14章の演習問題2で予告されていた限界です。

```rust
pub struct BufferPool {
    disk: DiskManager,
    frames: Vec<RwLock<Frame>>,
    inner: Mutex<Inner>,
}
```

この章では`frames`の要素を`RwLock<Frame>`に変え、`read_page`は`RwLock::read`を、`write_page`と`evict`、`flush_frame`は`RwLock::write`を取るようにしました。
この`RwLock<Frame>`こそが、この章が導入するLatchの実体です。
守っている対象(ページ本体)は第14章から変わっていません。
変わったのは、読み取り同士を同時に許すという一点だけです。

### Guardの解放順序に潜んでいた、実スレッドで初めて牙を剥くバグ

`BufferPool`には、フレーム本体を守る`RwLock`とは別に、`page_table`と`pin_count`をまとめて守る`Mutex<Inner>`があります(第14章)。
この2つのロックを両方とも触る箇所(`locate_or_load`と`evict`)は、常に「`Inner`を先にロックし、その`MutexGuard`を握ったままFrameのLatchを取る」という順序で統一されています。
逆順(Frameを先に、Innerをあとに)を許すと、スレッドAが`evict`のためにInnerを確保してFrameを待ち、スレッドBがFrameを確保したままunpinのためにInnerを待つ、という循環待ちが起こりえます。

ここで、`PageReadGuard`と`PageWriteGuard`の`Drop`を素朴に書くと、この逆順を自分から踏んでしまうことに気づきました。

```rust
impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        self.pool.unpin(self.frame_id, false);
    }
}
```

`unpin`(Inner確保)を先に呼んでいるように見えますが、Rustは構造体のフィールドを、`Drop::drop`の本体が終わった**あと**に宣言順で自動的にdropします。
つまりこの素朴な実装は、実際には「`unpin`(Inner確保)を実行 → その後でFrameの`RwLockReadGuard`を自動解放」という順序で動きます。
`unpin`の実行中、Frame Latchはまだ握ったままです。
これはモジュール冒頭の規律が禁じている逆順そのものであり、単一スレッドの間は誰も気づけません(同じスレッドの中で複数のGuardが同時に生きて競合することがないため)。
複数スレッドがBuffer Poolを本当に共有した瞬間、この逆順はデッドロックの芽になります。

この章では`guard`フィールドを`std::mem::ManuallyDrop`で包み、`Drop::drop`の中で明示的に順序を固定しました。

```rust
pub struct PageReadGuard<'a> {
    pool: &'a BufferPool,
    frame_id: usize,
    page_id: PageId,
    guard: ManuallyDrop<RwLockReadGuard<'a, Frame>>,
}

impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        // Frame Latchを先に解放してから`unpin`(Inner確保)を呼ぶ。
        unsafe {
            ManuallyDrop::drop(&mut self.guard);
        }
        self.pool.unpin(self.frame_id, false);
    }
}
```

`ManuallyDrop<T>`はTのデストラクタを自動では呼ばない、という一点だけを保証する型です。
`Drop::drop`の中で明示的に`ManuallyDrop::drop`を呼べば、その時点でFrame Latchが解放され、その後で`unpin`がInnerを確保します。
「Frameを先に手放してからInnerに触る」は、一見すると規律が定める「Innerを先に、Frameをあとに」と逆に見えます。
けれども規律が本当に禁じているのは、2つのロックを**同時に**逆順で持つことです。
このGuardのDropは、Frameを完全に手放し終えてからInnerを取るという、2つを同時に持たない書き方であり、規律とは矛盾しません(`PageWriteGuard`のDropも同じ理由で同じ形にしてあります)。

このバグは、単体テストでは一度も顕在化しませんでした。
単一スレッドの中では「AがFrameを持ったままInnerを待つ」相手のスレッドBがそもそも存在しないため、循環の片方が欠けたまま素通りします。
実スレッドの統合テストを書いて初めて拾える種類の不具合であり、この章が「実スレッド解禁」を目的に据える理由の一部でもあります。

## B+TreeのLock Coupling: 探索は親子で手をつなぎ、挿入は安全になったら手を放す

`BufferPool`のLatchはページ1枚の中身を守るだけで、複数ページにまたがる木の構造そのものを守ってはくれません。
`BTree`の`insert`が根から葉まで降りる途中、別のスレッドが同じ経路のどこかを書き換えていたら、探索は正しい葉にたどり着けません。
この章では、B+Treeの木構造を並行アクセスから守るために**Lock Coupling**(Crabbing、蟹のように親のハサミを閉じてから次のハサミを開く動きに由来する通称)を実装しました。

### 探索: 子のLatchを取ってから親を放す

`lookup`と`range`が使う探索(`find_leaf`と`find_leaf_for_lower_bound`)は、親のRead Latchを持ったまま子のRead Latchを取り、子を取ってから親を放します。

```rust
fn find_leaf(&self, key_bytes: &[u8]) -> DbResult<PageReadGuard<'_>> {
    let mut guard = self.pool.read_page(self.root)?;
    loop {
        match guard.page_type() {
            PageType::BTreeLeaf => return Ok(guard),
            PageType::BTreeInternal => {
                let next = InternalPageRef::open(guard.data())?.child_for(key_bytes);
                guard = self.pool.read_page(next)?;
            }
            other => return Err(unexpected_page_type(guard.page_id(), other)),
        }
    }
}
```

`guard = self.pool.read_page(next)?`という1行に、この規律のすべてが詰まっています。
右辺の`read_page(next)`が子のLatchを取り終えるまで、左辺への代入(親の`guard`の上書き、すなわち親のLatchの解放)は起こりません。
先に取ってから離す、という順序を1行の代入だけで実現しています。
逆の順序(親を先に放してから子を取る)を許すと、親を放した直後の隙間で、別のスレッドがまさにその親のページを書き換えてしまう余地が生まれます。

### 挿入: 悲観的Crabbing

`insert`は、根からWrite Latchを取りながら降り、通過したページを`ancestors`にスタックとして積みます。
各ノードに着いた時点で、このキーを収めても**そのノード自身がSplitして親へ伝播しないか**を判定し、安全だと分かればそれより上の祖先のLatchを全て解放します。

```rust
let mut ancestors: Vec<PageWriteGuard<'_>> = Vec::new();
let mut current = self.root;
loop {
    let guard = self.pool.write_page(current)?;
    match guard.page_type() {
        PageType::BTreeLeaf => {
            let safe = leaf_is_safe_for_insert(&LeafPageRef::open(guard.data())?, &key_bytes, rid);
            if safe {
                ancestors.clear();
            }
            ancestors.push(guard);
            break;
        }
        PageType::BTreeInternal => {
            let (next, safe) = {
                let view = InternalPageRef::open(guard.data())?;
                (view.child_for(&key_bytes), internal_is_safe_for_insert(&view, max_key_len))
            };
            if safe {
                ancestors.clear();
            }
            ancestors.push(guard);
            current = next;
        }
        other => return Err(unexpected_page_type(current, other)),
    }
}
```

「安全」の判定は2種類あります。
葉については、`(key_bytes, rid)`を実際に挿入した後のエントリ一覧を仮に組み立て、`leaf_entries_fit`で収まるかどうかを確認します。
挿入する値そのものが分かっているので、この判定は厳密です。

```rust
fn leaf_is_safe_for_insert(view: &LeafPageRef<'_>, key_bytes: &[u8], rid: RecordId) -> bool {
    let mut entries = view.entries();
    let pos = leaf_insert_position(&entries, key_bytes);
    entries.insert(pos, (key_bytes.to_vec(), rid));
    leaf_entries_fit(PAGE_PAYLOAD_SIZE, &entries)
}
```

内部ページについては、この時点ではまだ下の階層でSplitが起きるかどうかも、起きた場合に押し上げられてくる区切りキーの実際の長さも分かりません。
`max_key_len`(`insert`が受け付ける最大のキー長)を持つダミーのエントリを1件仮に足して判定することで、実際に来る区切りキーがどんな長さであっても安全側に倒します。

```rust
fn internal_is_safe_for_insert(view: &InternalPageRef<'_>, max_key_len: usize) -> bool {
    let mut entries = view.entries();
    entries.push((vec![0u8; max_key_len], PageId(0)));
    internal_entries_fit(PAGE_PAYLOAD_SIZE, &entries)
}
```

葉に着いた時点で`ancestors`に残っているのは、末尾(最も深い)が葉自身、それより前が実際にSplitしうる祖先だけです。
伝播は、この`ancestors`から都度`pop`したGuardをそのまま使い回します。

```rust
let mut current_guard = ancestors.pop().expect("...");
let mut pending = self.insert_into_leaf(&mut current_guard, &key_bytes, rid)?;

while let Some((separator, new_page_id)) = pending {
    let Some(mut parent_guard) = ancestors.pop() else {
        return self.grow_new_root(current_guard, &separator, new_page_id);
    };
    drop(current_guard);
    pending = self.insert_into_internal(&mut parent_guard, &separator, new_page_id)?;
    current_guard = parent_guard;
}
```

ページを指す`PageId`だけを覚えておいて、後から`pool.write_page`を呼び直す設計にはしませんでした。
すでにWrite Latchを握っているページを同じスレッドがもう一度`write_page`しようとすると、`RwLock`は再入可能ではないため、そのまま永久に止まります。
`ancestors`にGuardそのものを積んでおき、伝播のたびにそれを直接使い回す設計だけが、この自己デッドロックを避けられます。

### Split中の保護とRootの`PageId`が生涯変わらない理由

`split_leaf`と`split_internal`は、呼び出し元がすでにWrite Latchを握っているページ(`current_guard`)を直接書き換え、新しく確保する側のページだけを別途`write_page`します。
`current_guard`は関数の間ずっと同じLatchを保持したままなので、分割の左右どちらの書き込みも、他のスレッドの目に触れることなく完了します。

Rootが分割されるとき(`ancestors`が空になったとき)は、`grow_new_root`が木の高さを1つ増やします。
この章で、Root Splitの実装方針を根本から変えました。
第34章までのRoot Splitは、新しいInternal Pageを1枚確保してそちらを新しいRootに据え、`BTree`が持つ`root`フィールドをその新しいPageIdへ書き換える、という単純な設計でした。
単一スレッドではこれで何の問題もありません。
しかし複数スレッドが同時に`insert`していると、あるスレッドが`root`を読んで(Rootのつもりで)そのページへ向かい始めた直後に、別のスレッドがRootを分割して`root`を差し替えてしまう余地が生まれます。
前者のスレッドが実際にそのページへたどり着いた時点では、そこはもう「今のRoot」ではなく、Root Splitで左半分だけを残された**古い**Rootであり、右半分に移ったエントリが見えないまま誤ったページへ挿入してしまいます。
この章の統合テストを書く過程で、実際にこの経路の破損を観測しました。

そこでこの章では、**Rootの`PageId`を`create`のときのまま生涯変えない**ことにしました。

```rust
fn grow_new_root(&self, mut old_root_guard: PageWriteGuard<'_>, separator: &[u8], new_page_id: PageId) -> DbResult<()> {
    let left_child_type = old_root_guard.page_type();
    let left_child_id = self.pool.allocate_page(left_child_type)?;
    // 古いRootの中身を、新しく確保したページへそのままコピーする。
    // (LeafPageRef/InternalPageRefの内容をleft_child_idへ複製)
    ...
    // 古いRoot自身を、新しいInternal Page(1エントリだけ)へ上書きする。
    // page id(self.root)はここでも変わらない。
    old_root_guard.set_page_type(PageType::BTreeInternal);
    let mut page = InternalPage::init(old_root_guard.data_mut(), left_child_id);
    page.write_entries(left_child_id, &[(separator.to_vec(), new_page_id)]);
    Ok(())
}
```

Root Splitは、新しいページを1枚確保して**そちらへ古いRootの中身をコピーし**(`left_child_id`)、古いRoot自身のページ(page idはそのまま)を、`[left_child_id, (separator, new_page_id)]`という1エントリだけの新しいInternal Pageへ**上書き**します。
`root`を読むどのスレッドも、常に同じ`PageId`をLatchすればよく、その先の中身が「まだ葉のまま」か「すでにInternal Pageへ育っている」かだけが変わります。
`old_root_guard.set_page_type`が新しく必要になったのはこのためです。
ページの中身(`data_mut`が触るペイロード)とページの種類(`page_type`、ページヘッダの別フィールド)は別々に持たれているため、LeafからInternalへ実際に育てるには、中身の書き換えとは別にこの1行が要ります。

Rootの`PageId`が変わらなくなったことで、探索側にも1つ余地が残りました。
`range`が最初に見つけた葉のGuardを手放したあと、`RangeScan`(後述)が改めてその`PageId`を読みに行くまでの間に、木の高さがまだ1だったその葉が、他スレッドのRoot SplitでInternal Pageへ育ってしまうことがありえます。
この場合`RangeScan::next`は、想定していたLeaf Pageの代わりにInternal Pageを見つけ、出発点をもう一度探し直してから走査を続けます。
探し直す先は常に値(下限のキー)で決まるため、木の高さが1のRootは、`create`から一度もRoot Splitを経験していない場合に限りこの経路に入る、という以外の場所では起こりません。

## Latch取得順序によるデッドロック回避

Latchの取得順序は、常に**上から下、左から右**に固定しています。
上下の順序は、Root Split前後どちらの状態でも、Lock Couplingが根から葉へ降りる一本道そのものです。
複数スレッドがどの順でページに触れても、すでに持っているLatchより深い層のLatchだけを新たに要求する、という向きは変わりません。
左右の順序は、Range Scanが`next_leaf`(第24章)を辿って隣の葉へ進むときに現れます。
常に右隣のLatchだけを新たに取得し、左へ戻る経路を持ちません。

どちらの軸でも「すでに持っているLatchより上位か右側のLatchだけを新たに要求する」という規律が保たれている限り、2つのスレッドが互いに相手の持つLatchを待ち合う循環は起こりえません。
仮にスレッドAがページPを持ったままページQを待ち、スレッドBがページQを持ったままページPを待つ状況を考えると、この規律のもとではAがQを要求できるのはQがPより深いか右にある場合に限られ、Bが同時にPをQより深いか右にあるものとして要求することはできません。
順序が矛盾するペアは規律そのものが作れないため、循環待ちが構造的に成立しないという結論になります。

## Database層のスレッド対応: `SharedDatabase`と「待機」に変わったBlocked

`BufferPool`とB+Treeが本物のLatchを持つようになった一方で、`Database`自身(`Catalog`、`Backend`、`LockManager`等)はスレッドセーフになっていません。
この章では、`Database`全体を`Mutex`1本で包む最小限のラッパー`SharedDatabase`を追加しました。

```rust
pub struct SharedDatabase {
    db: std::sync::Mutex<Database>,
    cvar: std::sync::Condvar,
}
```

トランザクション同士の直列化はこの`Mutex`が丸ごと引き受け、ページ単位の細かい並行性は`BufferPool`とB+Treeが持つLatch(このラッパーとは別の粒度)が担います。
この二段構えは、SQL実行エンジン全体を細粒度にロックフリー化してセッションごとに独立させる(完全なSession分離、第37章)よりずっと粗いものですが、「複数スレッドから同じ`Database`を安全に触れる」という、この章の最小限の目標には足ります。

もう1つ、この章で変えたのは、`WouldBlock`(第31章)の扱いです。
[`LockManager::acquire`]自体は書き換えていません。
両立しないロックを見つけたら、`Blocked`という**値**を返すだけで、呼び出し元のスレッドを止めはしません。
決定的インターリーブテストハーネスは、この値を受け取って「今は再試行しない」と判断する側に回ることで、単一スレッドのままインターリーブを制御していました。

`SharedDatabase::execute_in_tx`は、同じ`DbError::WouldBlock`を受け取ったら`Condvar`でスレッドを実際に眠らせます。

```rust
pub fn execute_in_tx(&self, handle: &TxHandle, sql: &str) -> DbResult<QueryResult> {
    let mut guard = self.lock();
    loop {
        let outcome = guard.execute_in_tx(handle, sql);
        self.cvar.notify_all();
        match outcome {
            Err(DbError::WouldBlock) => {
                guard = self.cvar.wait(guard).unwrap_or_else(|p| p.into_inner());
            }
            other => return other,
        }
    }
}
```

試行のたびに(結果によらず)`notify_all`を呼んでいる点に、意図があります。
この試行がデッドロック解決のために別のトランザクションを強制Abortしていたら、そのVictim自身のスレッドが別に眠っているかもしれません。
通知を怠ると、そのスレッドは自分がAbort済みになったことに気付けないまま永久に眠り続けます。
過剰な通知(何も変わっていない試行のあとの通知)は、起こされたスレッドが条件を再確認して再び眠るだけで安全ですが、通知の欠落は起こすべきスレッドを永久に眠らせたままにします。
安全側に倒し、毎回無条件に通知しています。

`LockManager`本体(下の層)は値を返すだけの合議制のまま、`SharedDatabase`(上の層)がそれを待機に変える。
この二層構成のおかげで、`LockManager`は単一スレッドの決定的テスト(第30〜34章)と複数スレッドの実行時のどちらからも、1文字も変えずに使い回せています。

## 実スレッド統合テスト

`tests/concurrent_threads.rs`が、この章で解禁した実スレッドの並行テストです。
どのテストも、検証は**タイミングに依存しない決定的な最終状態**だけを見ます。
スレッドの実行順序そのものをアサートするテストは1つもありません。

複数スレッドが`Arc<BTree>`へ、互いに素なキー集合を同時に`insert`するテストは、`Barrier`で全スレッドの開始を揃え、木がまだ浅い段階から並行アクセスを集中させます。

```rust
let handles: Vec<_> = (0..THREADS)
    .map(|t| {
        let btree = Arc::clone(&btree);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            for i in 0..PER_THREAD {
                let key = t * PER_THREAD + i;
                btree.insert(&Value::BigInt(key), dummy_rid()).unwrap();
            }
        })
    })
    .collect();
```

検証は「全スレッドが挿入したキーが、すべて`lookup`で見つかる」ことと、`height() > 1`(十分な件数を挿入したのにRoot Splitが一度も起きていない、という事故を排除する)という、どちらも決定的な性質だけです。

`SharedDatabase`経由の統合テストは、複数スレッドが同じ行へ並行して`UPDATE ... SET v = v + 1`を行っても更新を1件も失わないことを確認します。
最終的な合計が`THREADS * ROUNDS`と正確に一致することが、Lost Updateが起きていない証拠になります。

デッドロック検出の実スレッド版は、T1が口座1→2、T2が口座2→1という逆順のロック取得を行うよう仕組み、`Barrier`で「両者とも1件目の更新を確定させた」直後に揃えてから2件目へ進ませることで、互いに相手の持つロックを待ち合う場面を必ず作ります。
Victim Selection(第32章)は循環の中で最も新しいTransactionIdを選ぶため、先に`begin_tx`した側が生き残り、後から`begin_tx`した側が必ずVictimになります。
この決定性を使って、実行順序を一切アサートせずに最終残高だけを検証しています。
実スレッドが本当にデッドロックしたまま検出されずに止まっていれば、このテスト自体がハングします。
ハングしたままCIを止めないよう、別スレッドからの完了通知に上限時間を設けました。

決定的インターリーブテストのハーネス(第30〜34章)は、この章のあとも1文字も変えずに緑のままです。
実スレッドを解禁したことは、既存の単一スレッドテストを置き換えるのではなく、その上に積み増すものだという位置づけを、テストスイート自体が示しています。

## この章の限界

Range Scanのイテレータ(`RangeScan`)は、`Iterator::next`の呼び出しの**外側**でLatchを持ち越しません。
ある`next()`呼び出しが終わってから次の`next()`が呼ばれるまでの間、呼び出し元が結果をどう使おうと、その間ずっとページをLatchし続けるのは、ロックをユーザーの考え中ずっと握るのと同じくらい具合が悪いためです。
この設計の代償として、走査の途中で他スレッドの`insert`によるSplitが割り込むと、走査中のスレッドは、Split前後の内容が混ざった順序で結果を観測することがあります。
個々の`next()`呼び出しはそれぞれ正しいLatchのもとで行われるため、壊れたページを読んだり`panic`したりすることはありません。
しかし走査全体を通した「常に昇順」という性質までは、この設計では保証しません。
書き込みがすべて確定したあとの走査は、常に厳密な昇順です(`tests/concurrent_threads.rs`で確認済み)。

`insert`の`unique`重複検査(`self.lookup(key)?.is_empty()`)は、この確認自体はLatchで守られていますが、確認してから実際に挿入するまでの間はLatchで一続きに保護されていません。
2つのスレッドが同時に同じキーを挿入しようとした場合、両方が「まだ存在しない」と判定してから両方が挿入を試み、片方だけが通ってもう片方は木の内部で偶然拒否される、あるいは稀に両方が通ってしまう、という余地が理論上は残ります。
この章の統合テストは、スレッドごとに互いに素なキー集合だけを挿入しており、この経路を突いていません。

`SharedDatabase`は`Database`全体を1本の`Mutex`で包む、意図的に粗い設計です。
`Catalog`、`LockManager`、実行計画の組み立てをページ単位のLatchのように細粒度化し、セッションごとに独立させる設計は、この章の範囲ではありません(第37章)。

**第5部の到達点**: 複数のトランザクションを実スレッドから並行に実行でき、Buffer PoolとB+Treeがページ単位のLatchで保護され、プロセスクラッシュ後にもコミット済みデータを復元できるRDBMSになりました。
第1章のロードマップが第5部の到達点として掲げていた「複数トランザクションを並行実行しつつ、クラッシュ後もコミット済みデータを復元できる」は、ここで実スレッドという最後の1ピースを含めて満たされたことになります。

## 演習問題

### 必須課題

1. `BTree::insert`の`unique`重複検査は、`self.lookup(key)?.is_empty()`という確認と、その後の`insert`本体が別々のLatch取得で行われています(「この章の限界」を参照)。同じキーを複数スレッドから同時に挿入するテストを書き、実際に何が起こるか観察してください。壊れずに片方だけが`DbError::BTreeUniqueViolation`になるとしたら、それはこの章のどの仕組みが偶然救っているからか、コードを読んで説明してください。
2. `RangeScan`のLatchは`next()`の呼び出しの外側へ持ち越さない設計です(「この章の限界」を参照)。仮に`RangeScan`自身が現在の葉のRead Latchを`next()`をまたいで保持し続ける設計に変えたとして、その設計がどんな新しい問題(デッドロックのリスク、他スレッドの長時間ブロック)を持ち込むか、具体的なシナリオを1つ考えて説明してください。
3. `SharedDatabase::execute_in_tx`は毎回無条件に`notify_all`を呼びます。この呼び出しを「`WouldBlock`だったときだけ」に変更すると、`tests/concurrent_threads.rs`の`deadlock_between_two_real_threads_is_detected_and_resolved`がハングしうることを、実際にコードを変更して確認してください。ハングする条件を、Victim Selectionが誰の攻撃で解決されるかに注目して説明してください。

### 発展課題

1. この章の`insert`は悲観的Crabbing(Write Latchを取ってから安全性を判定する)を採用しています。楽観的Crabbing(まずRead Latchだけで安全そうな葉まで降り、実際に書き込む段になってからWrite Latchへ昇格し、昇格に失敗したら最初からやり直す)を実装してください。`std::sync::RwLock`は読み取りロックから書き込みロックへの直接の昇格を提供しないため、昇格の実現方法(いったん手放して取り直す、`try_write`で非ブロッキングに試す等)から設計する必要があります。
2. Blink-treeを調べ、この章の「走査全体を通した昇順を保証しない」という限界(「この章の限界」を参照)がBlink-treeでどう解消されるか説明したうえで、`BTree`に最小限のBlink-tree的な仕組み(各ページに持たせる高いキーの上限、Split中の一時的な右リンク)を実装してください。
3. `BufferPool`の`evict`(Clock置換)は、`Inner`のMutexを握ったままダーティなページの書き戻し(ディスクI/O)を行います(`crate::buffer_pool`モジュールドキュメントを参照)。この間、他のどのスレッドも`BufferPool`の`read_page`と`write_page`を呼べません。ディスクI/Oのレイテンシがボトルネックになる場面を想定し、書き戻し中はInnerのLockを手放せるように設計を変更してください。書き戻し中に同じフレームへの新たな要求が来た場合の扱いから考える必要があります。
