# 第40章 テスト、Fuzzing、Benchmark、リリース

第39章までで、`minidb`はTCP経由でクライアントから接続できるClient/Server型のRDBMSになりました。
SQLサブセットの実行、ディスクへの永続化、Join、集約、コストベースの最適化、トランザクション、Crash Recovery、System Catalog、どれも一通り動きます。
機能の一覧としては、この教材が第1章で掲げた到達点をすでに満たしています。

けれど、こう問い直すとどうでしょうか。
`minidb`は正しいと、どこまで自信を持って言えるでしょうか。

「テストが全部通っている」という答えは、半分しか答えになっていません。
`tests/`配下には第3章のGolden Testから第36章のWire Protocol Testまで、10本を超える統合テストファイルが積み上がっています。
それぞれは対応する章の主張(このロックが競合を防ぐ、この復旧手順がクラッシュを取り消す)をそのときどきの視点で確認したものであり、章をまたいで見比べる目的では書かれていません。
どのテストがどの層を守っているのか、逆にどの層がまだ手薄なのかは、実は誰も一望していません。

この章はその一覧を作ることから始めます。
既存の資産を並べ直したうえで、手薄な層(パーサへの未知の入力、ランダムな操作列、クラッシュのタイミング、ランダムな並行実行)を実際に埋めていきます。
最後に、この教材が積み上げてきた全40章を実装として動かすサンプルアプリケーションと、データベースファイルの形式と互換性方針の文書化で締めくくります。

## テスト資産の全体地図

まず、今`minidb`が持っているテストの層を並べます。

| 層 | 確認すること | 主なファイル | 導入した章 |
| --- | --- | --- | --- |
| Unit Test | 各モジュールの契約(境界値、エラー系) | `src/*.rs`内の`#[cfg(test)]` | 第3章から各章 |
| SQL Golden Test | SQL文と出力の対応(回帰) | `tests/golden/*.sql`、`tests/golden.rs` | 第3章 |
| SQLite Differential Test | SQLiteとの出力一致 | `tests/differential.rs` | 第10章 |
| File Checksum Test | 壊れたPageとFile Headerの検出 | `src/page.rs` | 第11章 |
| Persistence Test | 再起動をまたいだ永続化 | `tests/persistence.rs` | 第16章 |
| 決定的Interleave Test | 4つの異常(Lost Update等)の有無 | `tests/interleave.rs`、`tests/interleave_disk.rs` | 第30〜32章 |
| Isolation Level Test | 分離レベルごとの許容と防止 | `tests/isolation_levels.rs` | 第32章 |
| Deadlock Test | 検出とVictim Selection | `tests/deadlock.rs` | 第32章 |
| WAL Durability Test | COMMIT後の耐久性 | `tests/wal_durability.rs` | 第33章 |
| Crash Recovery Test | 固定位置のクラッシュからの復元(シナリオ別) | `tests/crash_recovery.rs` | 第34章 |
| 実スレッドConcurrency Test | 実OSスレッドでの競合 | `tests/concurrent_threads.rs` | 第35章 |
| Wire Protocol Test | TCP経由の往復 | `tests/wire_protocol.rs` | 第36章 |
| Parser Fuzzing | ランダムな入力で`panic`しないか | `tests/fuzz_parser.rs` | 第40章 |
| Property-based Test(B+Tree) | ランダムな操作列と`BTreeMap`モデルの一致 | `src/btree.rs` | 第23章のモデルテストを第40章で拡張 |
| Property-based Test(SQL) | 行数と合計の不変条件 | `tests/property_sql.rs` | 第40章 |
| Crash Injection Loop | ランダムな位置でのクラッシュから復元 | `tests/crash_loop.rs` | 第34章のシナリオを第40章で一般化 |
| 決定的Random Concurrency | ランダムなインターリーブでの直列化可能性 | `tests/concurrent_random.rs` | 第30章のハーネスを第40章で拡張 |
| Benchmark | 実行時間の回帰の目安 | `tests/bench.rs` | 第40章 |

こう並べると、手薄だった層がどこかが見えてきます。
上から12段目までは、著者が具体的なシナリオを1つずつ選んで書いたテストです。
「COMMIT直後にプロセスが死んだらどうなるか」のような、特定の状況を狙い撃ちしています。
けれど、著者が思いつかなかった状況はテストに現れません。
パーサに想定外のトークン列を投げたらどうなるか、B+Treeへの挿入と削除をでたらめな順序で繰り返したらどうなるか、Crash Recoveryのちょうど途中で止めたらどうなるかは、シナリオを手で選ぶ書き方とは相性が悪い問いです。
残りの6段が、この章でランダム化によって埋める層です。

## Parser Fuzzing: 自作の軽量ファザー

パーサへの未知の入力を大量に試す手法としては、libFuzzerを使う`cargo-fuzz`がよく使われます。
ただし`cargo-fuzz`はASan(AddressSanitizer)を要求し、Nightly Rustのツールチェインに依存し、コーパスの保存先もOS依存です。
この教材のCIは安定版のRustと`cargo test`だけで完結させる方針を採ってきており、章の主題(テスト資産の拡充と統合)に対して環境構築の手間が見合いません。
そこで、依存クレートを増やさずシードを固定できる自作の疑似乱数ファザーを採用します。

`Xorshift64`という決定的な疑似乱数生成器は、実は本書に初めて出てくるものではありません。
第23章の`src/btree.rs`、第12章の`src/slotted_page.rs`のテストモジュールが、ランダムな挿入順序を再現可能に作るためすでに使っています。
この章はその実装を、新規に作成する`tests/common/mod.rs`へ集約し、Fuzzing、Property-based Test、Crash Injection Loop、決定的Random Concurrencyの4つの新しいテストファイルで共有します。

```rust
pub struct Xorshift64(pub u64);

impl Xorshift64 {
    pub fn new(seed: u64) -> Self {
        // 種が0だとxorshiftは0を返し続けて壊れるため、0除けの奇数へ倒す。
        Xorshift64(seed | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn range(&mut self, bound: usize) -> usize {
        (self.next() as usize) % bound
    }

    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.next() % den < num
    }
}
```

新規に作成する`tests/fuzz_parser.rs`は、この乱数を使う2種類の生成器で`tokenize`と`parse_statement`に入力を投げ込みます。

**バイト列ファザー**は、完全にランダムなバイト列を`String::from_utf8_lossy`で文字列化するだけです。
不正なUTF-8由来の置換文字や制御文字、突然終わる入力など、字句解析器の境界を無差別に突きます。

**トークン列ファザー**は、キーワード、識別子、数値、文字列リテラル、記号の辞書からランダムに選んで並べます。
文法として正しい保証はありませんが、バイト列ファザーより構文解析器の奥(式、`JOIN`、`CAST`、`PREPARE`/`EXECUTE`)まで届きやすくなります。
`tests/fuzz_parser.rs`に次の`generate_token_soup`を定義します。

```rust
fn generate_token_soup(rng: &mut Xorshift64, len: usize) -> String {
    let mut out = String::new();
    let mut paren_depth = 0usize;
    for i in 0..len {
        if i > 0 {
            out.push(' ');
        }
        let bucket = rng.range(6);
        let piece = match bucket {
            0 => KEYWORDS[rng.range(KEYWORDS.len())],
            1 => {
                if paren_depth < MAX_PAREN_DEPTH && rng.chance(1, 2) {
                    paren_depth += 1;
                    "("
                } else if paren_depth > 0 {
                    paren_depth -= 1;
                    ")"
                } else {
                    PUNCTUATION[rng.range(PUNCTUATION.len())]
                }
            }
            2 => PUNCTUATION[rng.range(PUNCTUATION.len())],
            3 => IDENTS[rng.range(IDENTS.len())],
            4 => STRINGS[rng.range(STRINGS.len())],
            _ => NUMBERS[rng.range(NUMBERS.len())],
        };
        out.push_str(piece);
    }
    for _ in 0..paren_depth {
        out.push_str(" )");
    }
    out
}
```

丸括弧の入れ子を`MAX_PAREN_DEPTH`(12)で浅く抑えてあるところに、ランダムファザーの限界が現れています。
構文解析器は再帰下降なので、丸括弧を数万重ねると`panic`ではなくスタックオーバーフローでプロセスごと落ちます。
`std::panic::catch_unwind`で捕まえられるのは`panic`だけで、スタックオーバーフローは捕まえられません。
見つけたら測定中のテストプロセスごと道連れにする類の異常を、無差別に踏みにいく設計にはできませんでした。
再帰の深さに明示的な上限を持たせる修正は構文解析器自体(第7章)の変更になるため、この章の対象(テスト資産の拡充と統合)から外れます。
章末の演習で扱います。

判定はどちらの生成器でも同じで、`catch_unwind`で包んで`panic`しないことだけを見ます。
`Err`を返すのは正常系(壊れた入力を拒否できた)として扱う`assert_no_panic`を、`tests/fuzz_parser.rs`に次のように定義します。

```rust
fn assert_no_panic(input: &str) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = tokenize(input);
        let _ = parse_statement(input);
    }));
    assert!(result.is_ok(), "panicした入力: {input:?}");
}
```

2つの生成器をそれぞれ1万ケースずつ走らせても、この章の執筆時点で`panic`は見つかりませんでした。
これは字句解析器と構文解析器の頑健性がすでに高い(第6章と第7章がエラー系のUnit Testを丁寧に積んできた成果)ことの傍証であり、Fuzzingが無駄だったという意味ではありません。
バイト列ファザーとトークン列ファザーのどちらも、シードを変えれば別の入力を試します。
今回何も見つからなかったことは「この2つのシードでは見つからなかった」以上を意味しません。

一方で、このランダム生成が再現するのは、コードを変えない場合に限られます。
シード値そのものは`Xorshift64::new(0xf0f0_1234_5678_9abc)`のように固定してあるので、同じコードを実行すれば同じ入力列が何度でも再現します。
しかし`generate_token_soup`や`generate_byte_soup`という生成ロジック自体を変更すれば、同じシード値であっても生成される入力列は変わってしまいます。
「過去に見つかった具体的な入力」を、シードや生成ロジックの変更を経ても確実に踏み続けたいなら、どちらにも依存しない固定の文字列として残しておく必要があります。
`regression_corpus_does_not_panic`は、空入力、丸括弧の過不足、`i64`の上下限をまたぐ数値、閉じない文字列やコメント、`$`パラメータの異常値など、辞書の組み合わせだけでは作りにくい境界を人手で選んだ固定コーパスです。
今後のレビューで実際に`panic`する入力が見つかれば、シード値ではなくこの文字列のリストへ追記します。

## Property-based Test: ランダムな操作列とモデルの一致

Fuzzingが「壊れないこと」だけを見るのに対し、Property-based Testは「何が成り立つべきか」という不変条件を、ランダムな入力に対して確認します。
この教材にはすでに1つの実例があります。
第23章の`src/btree.rs`が、`BTree::insert`の結果を`std::collections::BTreeMap`という「正しいと分かっている」実装と突き合わせるモデルテストです。

既存のモデルテストには弱点がありました。
`large_seeded_random_insert_matches_a_btreemap_model`は挿入だけ、`delete_then_lookup_and_range_agree_after_a_seeded_random_workload`は「全部挿入してから半分削除」という2フェーズの構成です。
木の中身が挿入と削除の混在で絶えず変化し続ける状態そのものは、どちらのテストも再現していません。
分割、併合、借用の実装が、ある特定の挿入と削除の混在パターンの直後だけ壊れるという種類のバグは、フェーズを分けたテストでは踏めません。

そこでこの章は、`insert`と`delete`と`range`を1ステップごとにランダムへ混ぜ、**すべてのステップの直後**に`BTreeMap`と一致することを確認するテストを、`src/btree.rs`の`#[cfg(test)]`モジュールに追加します。

```rust
for step in 0..3_000usize {
    match rng.next() % 10 {
        0..=5 => {
            let key = (rng.next() % key_space as u64) as i64;
            if model.contains_key(&key) {
                continue;
            }
            let record = rid((step as u64 % 1000) + 1, (step % 100) as u16);
            btree.insert(&Value::BigInt(key), record).unwrap();
            model.insert(key, record);
            assert_eq!(btree.lookup(&Value::BigInt(key)).unwrap(), vec![record], "step={step} key={key}");
        }
        6..=8 => {
            if model.is_empty() {
                continue;
            }
            let idx = (rng.next() as usize) % model.len();
            let key = *model.keys().nth(idx).unwrap();
            let record = model[&key];
            assert!(btree.delete(&Value::BigInt(key), record).unwrap(), "step={step} key={key}");
            model.remove(&key);
            assert_eq!(btree.lookup(&Value::BigInt(key)).unwrap(), Vec::new(), "step={step} key={key}");
        }
        _ => {
            let lo = (rng.next() % key_space as u64) as i64;
            let hi = (rng.next() % key_space as u64) as i64;
            let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
            let found = collect_range(&btree, Bound::Included(&Value::BigInt(lo)), Bound::Included(&Value::BigInt(hi)));
            let expected: Vec<(i64, RecordId)> = model.range(lo..=hi).map(|(&k, &v)| (k, v)).collect();
            assert_eq!(found, expected, "step={step} range={lo}..={hi}");
        }
    }
    if step % 100 == 0 {
        let all_via_range = collect_range(&btree, Bound::Unbounded, Bound::Unbounded);
        let expected_via_model: Vec<(i64, RecordId)> = model.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(all_via_range, expected_via_model, "step={step}");
    }
}
```

毎ステップ全キーを突き合わせるのはコストが高いので、個々のキーの整合(挿入または削除した直後のそのキーの`lookup`)はステップごとに確認し、木全体の整合は100ステップに1回だけ確認します。
確認の粒度を落としても、崩れたキーが1つでもあれば次にそのキーへ触れた瞬間の`assert`で捕まる設計です。

もう1つのPropertyは、B+Treeという1層ではなく、SQL文を積み重ねた結果に対するものです。
個々のSQL文が正しくても、束ねたときに崩れる不変条件はありえます。
新規に作成する`tests/property_sql.rs`は2つのPropertyを確認します。

1つ目は行数の不変条件です。
`INSERT`した行数と`DELETE`した行数の差は、常に`COUNT(*)`と一致するはずです。
挿入と削除をランダムに混ぜた50通りの操作列それぞれについて、1操作ごとにこの一致を確認します。

2つ目は合計の不変条件です。
口座間の送金(ある行から引いた分だけ別の行へ足す)を何回繰り返しても、`SUM(balance)`は変わらないはずです。
この送金を1回分実行する次のコードを、`tests/property_sql.rs`に書きます。

```rust
db.execute("BEGIN").unwrap();
db.execute(&format!("UPDATE accounts SET balance = balance - {amount} WHERE id = {from}")).unwrap();
db.execute(&format!("UPDATE accounts SET balance = balance + {amount} WHERE id = {to}")).unwrap();
if will_rollback {
    db.execute("ROLLBACK").unwrap();
} else {
    db.execute("COMMIT").unwrap();
    model[from as usize] -= amount;
    model[to as usize] += amount;
}
```

3割の確率で`ROLLBACK`することも混ぜてあります。
`ROLLBACK`した回だけは合計にも個別の残高にも反映されないことを、Rust側に持つ「コミット済みの残高だけを反映する台帳」と突き合わせて確認します。

## Crash Injectionの一般化: ランダムな位置で壊す

第34章の`tests/crash_recovery.rs`シナリオ(c)は、`crate::failpoint`を使い「1つ目のloserトランザクションをUndoし終えた直後」という1つの決まった位置でRecovery自身を失敗させ、2回目の`Database::open`が最初からやり直して完走することを確認していました。
決まった位置は1つしかないので、Redoの1件目で死んだ場合、5件目で死んだ場合、Undoの直前で死んだ場合をそれぞれ試そうとすると、シナリオを増やすたびにテストを書き足す必要があります。

新規に作成する`tests/crash_loop.rs`は、この位置をシードから決定的に選び直す形へ一般化します。
1回のイテレーションは次の手順を踏みます。

1. シードから決定的な送金ワークロード(コミット済みの送金を何本かと、最後に1本だけ残す未コミットの送金)を組み立て、`flush`しないまま`Database`を`drop`してクラッシュを模す。
2. 同じシードでもう一度同じワークロードを別のファイルへ組み立て、今度は`crate::failpoint`で`recovery_redo_step`か`recovery_undo_step`のどちらか一方に、実際に踏まれる回数の範囲内でランダムな発火位置を仕込む。
3. 1回目の`Database::open`がその位置で失敗することを確認する。
4. 2回目の`Database::open`(failpointは発火すると自動でdisarmされる)が完走し、コミット済みの送金だけが反映され、未コミットの送金は跡形もなくUndoされていて、口座の合計金額(送金では変わらないはずの不変条件)が保存されていることを確認する。

`tests/crash_loop.rs`の該当部分は次のようになります。

```rust
let (failpoint_name, count) = if rng.chance(1, 2) {
    ("recovery_redo_step", 1 + rng.range(records_scanned))
} else {
    ("recovery_undo_step", 1usize)
};

failpoint::arm(failpoint_name, count);
assert!(Database::open(&armed_path).is_err(), "seed={seed} {failpoint_name}の{count}回目で失敗するよう仕込んだ");

let db = Database::open(&armed_path).unwrap();
```

同じワークロードを2回組み立て直しているのは、`Database::open`がRecoveryに成功すると`flush`、`sync`、索引の`rename`でディスク上の状態を書き換えてしまい、1回目に使ったファイルではもう「クラッシュ直後」のバイト列を再現できないためです。
発火位置を選ぶ範囲(`records_scanned`)は、1回目の乾式実行(failpointを仕込まない`Database::open`)が返す`RecoveryReport`の実測値から決めています。
当てずっぽうの範囲を決め打ちすると、実際には起こりえない回数を指定して意味の無い成功ケースを量産しかねないためです。

シードを12通り走らせるテストはCIの時間内に収まる規模に抑えてあり、`#[ignore]`を付けたもう1本のテストは、ランダムな1点ではなくRedoの全ステップを1つずつ狙って失敗させる網羅版です。
`cargo test --release --test crash_loop -- --ignored --nocapture`で手元から実行できます。

## 決定的Random Concurrency: ランダムなインターリーブでも直列化可能性は保たれるか

第30〜32章の決定的インターリーブテストは、2本のトランザクションの操作順を著者が手で1通り選び、その1通りについて異常の有無を確認するものでした。
新規に作成する`tests/concurrent_random.rs`はその手作業を、シードから決定的に選んだランダムな操作順へ置き換えます。
狙いは異常の有無ではありません。
Strict 2PL(第31章)が主張する性質そのもの、実際にどう入り組んで実行されても結果はどれかの直列実行と一致するという直列化可能性が、たくさんの順序を試しても崩れないことです。

2本のトランザクションT1とT2が、2行(`id=1`、`id=2`)の`tag`列に自分の名前を書き込みます。
上書きなので、最後にどちらが書いたかで結果が変わる非可換な操作です。
書き込み先の行の順序も、どちらを先に試すかも、シードごとにランダムなスケジューラが決めます。
`tests/concurrent_random.rs`のスケジューラは、次のように各Runnerの実行結果で分岐します。

```rust
match db.execute_in_tx(&runner.handle, &runner.next_sql()) {
    Ok(_) => {
        runner.pos += 1;
        if runner.done() {
            db.commit_tx(runner.handle).unwrap();
            commit_order.push(runner.name);
        }
    }
    Err(DbError::WouldBlock) => {
        // ロックが空くまで、この文はまだ実行されていない。
        // 次のtickで別のRunnerが先に進むのを待つ。
    }
    Err(DbError::DeadlockDetected) => {
        // Victimになった側は、同じ行順序で最初からやり直す。
        db.rollback_tx(runner.handle).ok();
        let name = runner.name;
        let row_order = runner.row_order;
        *runner = Runner::start(&mut db, name, row_order);
    }
    Err(other) => panic!("seed={seed}: 想定外のエラー: {other:?}"),
}
```

判定の根拠はStrict 2PLの定理そのものです。
競合する2本のトランザクションのうち後から`COMMIT`が通った方が、実際の実行としても後に直列実行されたことになります。
先にCOMMITした方が持っていたロックを、後にCOMMITする方は`COMMIT`の直前まで待たされてから引き継ぐしかないためです。
したがって、実際に最後にCOMMITしたトランザクションの名前を両方の行が持っていることを確認すれば、直列化可能性を壊す実行が無かったと言えます。

デッドロックはVictim Selection(第32章)で自動的に解消されるので、Victimになった側をこのテストのスケジューラがその場で`ROLLBACK`して、同じ行順序で最初からやり直します。
シードを60通り試しても、最終的に2行が別々のトランザクションの名前を持つ(直列化可能性の違反)ケースは見つかりませんでした。

## Benchmark: 回帰の目安として記録する

Benchmarkだけは、正しさではなく速さを扱います。
criterionのような専用クレートは依存を増やすため採らず、第24章の`lookup_time_grows_much_slower_than_table_size`がすでに使っていた形、`Instant`で測って`println!`するだけの`#[ignore]`テストへ統一しました。
実行環境(CPU、ディスク、他プロセスの負荷)に左右される実測値そのものを、数値目標として`assert`することはしません。
ここでの役割は回帰の目安です。
章を書き足すたびに`cargo test --release --test bench -- --ignored --nocapture`を手元で走らせ、直前の実行結果と比べて極端に遅くなっていないかを確認する運用を想定しています。

`tests/bench.rs`は4種類のベンチマークを持ちます。

- **Micro Benchmark**: Point Lookup(`WHERE id = ...`)、Range Scan(`WHERE id >= ... AND id <= ...`)、Insert throughput(`BEGIN`でまとめた場合とAutocommitの場合を並べる)
- **Join / Aggregate Benchmark**: `customers`500件と`orders`8,000件をHash Joinで結合し、`GROUP BY`で集計する
- **小規模OLTP Benchmark**: 口座間の送金(`BEGIN`、`UPDATE`を2回、`COMMIT`)を直列に繰り返す、TPC-Bの送金1本ぶんを単一スレッドへ切り詰めた形

このベンチマークを実際に書いて走らせる過程で、1つの制約に行き当たりました。
`id`と`v`の2列だけを持つ細いテーブルへ数百行を超えて挿入し続けると、2枚目以降のページに空きがあるにもかかわらずFree Space Mapが候補として拾えなくなり、ページが際限なく増えて661件目に`DbError::CatalogTooLarge`(第15章)へ突き当たります。

```text
カタログがページに収まりません: 4076バイト(上限4072バイト)
```

3列目を1つ足すだけでこの制約を踏まずに済むことも確認できました。
原因の特定と修正は第15章のFree Space Mapが対象とする範囲であり、この章(テスト資産の拡充と統合)の範囲を超えるため、深追いはせずベンチマーク側のテーブル定義を3列にして回避し、章末の演習に送ります。
数値目標を立てずに実測だけを記録する方針のこの章にとって、実測している最中に実際の制約へ行き当たったこと自体が、Benchmarkという層の存在意義を裏付けています。

実測の一例です(手元の開発機、`--release`ビルド)。

```text
point_lookup n=  20000 total=9.127121294s avg=45.635606ms
range_scan   n=  20000 width=   200 total=4.694381479s avg=93.887629ms
insert n=   5000 mode=batched   total=783.750791ms avg=156.75µs
insert n=   5000 mode=autocommit total=6.989962628s avg=1.397992ms
join_aggregate customers=500 orders=8000 rows=500 elapsed=61.656381ms
oltp_transfer accounts=100 transfers=2000 total=6.6200069s avg=3.310003ms tps=302
```

`insert`の2行を見比べると、`BEGIN`でまとめたバッチ挿入がAutocommitのおよそ9倍速いことが分かります。
第33章で確認したとおり、Autocommitの1文はそれ自体が1トランザクションであり、`COMMIT`のたびにWALの`sync`が走ります。
`BEGIN`でまとめれば、`sync`はまとめの最後の`COMMIT`1回だけで済みます。
この差はこの章で初めて測ったものではなく、第33章の本文でも述べた設計の帰結です。
ベンチマークという層は、その帰結を具体的な倍率として毎回同じ形で再確認できるようにします。

## サンプルアプリケーション: ToDoリストで一通り使う

テストは実装の正しさを確認しますが、実装の使い方そのものは示しません。
新規に作成する`examples/todo.rs`は、この教材が積み上げてきたEmbedded APIを、1つのToDoリストアプリケーションとして動かします。
`examples/todo.rs`は`cargo run --example todo`で実行でき、次のように書き始めます。

```rust
let shared = Arc::new(SharedDatabase::new(Database::memory()));
let mut session = Session::new(Arc::clone(&shared));
```

`Database`を直接使わず`SharedDatabase`と`Session`から組み立てているのは、`PREPARE`、`EXECUTE`、`DEALLOCATE`が`Database::execute`では受け付けられず、`Session`を経由する必要があるためです(第37章)。
サーバー(第36章)を経由しない、単一プロセス内のEmbedded APIとしての使い方であり、`Session`はサーバーだけのものではありません。

このサンプルは次を順に実行します。

1. `CREATE TABLE`でリスト(`lists`)とToDo(`todos`)の2テーブルを作る(第9章)
2. `PREPARE add_todo AS INSERT INTO todos VALUES ($1, $2, $3, FALSE)`でPrepared Statementを用意し、`EXECUTE add_todo(...)`で複数件のToDoを追加する(第37章)
3. `UPDATE`で1件を完了にする(第10章)
4. `JOIN`でToDoをリスト名つきに結合し、`GROUP BY`でリストごとの件数を集計する(第21章と第22章)
5. `BEGIN`/`COMMIT`で複数の`UPDATE`を1つの単位にまとめる(第30章)
6. `BEGIN`してから`DELETE FROM todos`し、`ROLLBACK`で取り消す。コミットする前に取り消せることを、`COUNT(*)`の変化で確認する
7. `DEALLOCATE`でPrepared Statementを手放す

6番目の手順は、単に機能を並べるだけでなく、トランザクションという仕組みの効能を1つの場面に落とし込んだものです。
全件削除という取り返しのつかなさそうな操作も、`COMMIT`するまでは確定しません。

## ファイル形式のレイアウトと互換性方針

この教材が作るデータベースファイルの形式は、章ごとに次のレイヤーへ積み上がってきました。

| レイヤー | 内容 | バージョン管理 | 導入した章 |
| --- | --- | --- | --- |
| File Header | `magic`、`format_version`、`page_size`、`page_count`、checksum | `FORMAT_VERSION`(現在2) | 第11章 |
| Page Header | `page_id`、`page_type`、`page_lsn`、checksum | `FORMAT_VERSION`と共通 | 第11章と第34章 |
| Catalog Page | テーブル定義、Free Page List、索引メタデータ、統計情報 | `CATALOG_MAGIC`と`CATALOG_LAYOUT_VERSION`(現在3) | 第15章、第20章、第24章、第27章 |
| WALファイル(`<db_path>.wal`) | Begin/Insert/Update/Delete/Commit/Abort/Checkpointの各ログレコード | Page Headerと同じ`FORMAT_VERSION`圏外、独自の追記専用形式 | 第33章 |
| 索引ファイル(`<db_path>.<index_name>.idx`) | B+Treeのルートページ、内部ページ、葉ページ | Page Headerと共通のPage形式 | 第24章 |

`FORMAT_VERSION`はPageとFile Headerという「バイト列の外枠」だけを保証する番号で、Catalog Pageの`payload`内部のレイアウトまでは関知しません。
だからこそCatalog Page専用に`CATALOG_MAGIC`と`CATALOG_LAYOUT_VERSION`という別のバージョン番号を持たせています(第15章、`src/storage.rs`モジュール冒頭)。
この2系統のバージョン番号が今どちらも指しているのは、この章の時点のレイアウトだけです。

この教材は、章をまたいだデータベースファイルの互換性を約束しません。
各章は`git`タグ(`chapter-XX-start`と`chapter-XX-final`、第1章)で区切られた1つのスナップショットであり、`Storage::open`が読めるのは同じ章の`Storage::create`(またはレイアウトを変えていない章)が書いたファイルに限られます。
古い章で作ったファイルを新しい章のコードで開こうとすると、`format_version`または`CATALOG_LAYOUT_VERSION`の不一致で`DbError::CorruptPage`か`DbError::CorruptCatalog`のどちらかに確実に倒れます。
静かに壊れたデータを読み込んでしまうより、開けないと分かる方を選ぶという、この教材が第11章から一貫して採ってきた方針です。
この方針は最終章になっても変わっていません。
変わっていないこと自体を、この章であらためて明文化しました。

## テスト

`tests/fuzz_parser.rs`は、バイト列ファザーとトークン列ファザーをそれぞれ1万ケース、固定コーパスを追加で走らせ、`tokenize`と`parse_statement`のどちらも`panic`しないことを確認します。

`src/btree.rs`の`random_interleaved_insert_delete_range_matches_a_btreemap_model`は、`insert`と`delete`と`range`を3,000ステップ混ぜ、個々のステップの直後と100ステップに1回の全件比較の両方で`BTreeMap`モデルと一致することを確認します。

`tests/property_sql.rs`は、行数の不変条件を50通りのシードで、口座間送金の合計の不変条件を30通りのシードでそれぞれ確認します。

`tests/crash_loop.rs`は、RedoかUndoのどちらかにランダムな発火位置を仕込んだRecoveryの失敗と、その後の完全な復元を12通りのシードで確認します。
`#[ignore]`を付けたもう1本は、Redoの全ステップを網羅する重い版です。

`tests/concurrent_random.rs`は、2本のトランザクションのランダムなインターリーブが常に実際の`COMMIT`順と一致した結果を残すことを、60通りのシードで確認します。

`tests/bench.rs`は5本の`#[ignore]`ベンチマークとして、既存の`src/btree.rs`のスケーリング測定と合わせて回帰の目安を提供します。

`examples/todo.rs`はテストではありませんが、`cargo run --example todo`が最後まで`panic`せず完走することを、この章のCIチェックに含めています。

## この章の限界

Parser Fuzzingは`cargo-fuzz`(libFuzzer)を採らなかったため、Rustコンパイラの外側にあるクラッシュ(スタックオーバーフロー、メモリ安全性のバグ)を検出する能力は持ちません。
自作のファザーが確認できるのは「Rustの`panic`機構が捕まえられる異常が起きないこと」だけです。

決定的Random Concurrencyは2本のトランザクション、2行という小さな構成に留めています。
3本以上のトランザクションが絡む循環的な待ち合いや、行の追加(Phantom、第32章)を含むインターリーブは、スケジューラの実装が一段複雑になるため対象外にしました。

Benchmarkは実行環境に依存する実測値をそのまま記録するだけで、統計的な有意性検定(複数回の実行のばらつきを踏まえた比較)は行っていません。
継続的にベンチマーク結果を記録し、コミットのたびに自動で比較するCI連携も、この章では組み込んでいません。

Benchmarkを書く過程で見つかったFree Space Mapの制約(狭いテーブルでページ数が際限なく増える不具合)は、原因を特定した段階で留め、修正はしていません。
この教材は各章の対象範囲を厳密に区切る方針を採っており、Benchmarkのためのテストコードから、対象範囲の異なる第15章の実装へ手を伸ばすことは避けました。

## 到達点

第1章のロードマップが掲げていた7部構成の到達点を、最後にもう一度並べます。

| 部 | 到達点 | 対応する章 |
| --- | --- | --- |
| 第0部 設計と開発環境 | 開発の土台(エラー型、テスト基盤)が揃う | 第1〜3章 |
| 第1部 Bare Bones | ディスクを使わないインメモリSQLデータベースが動く | 第4〜10章 |
| 第2部 Storage | プロセスを再起動してもテーブルとデータが残り、SQLレベルで動作するディスクRDBMSになる | 第11〜16章 |
| 第3部 Query Execution | 複数テーブルのJoin、集約、インデックス検索がSQLから実行できる | 第17〜25章 |
| 第4部 Query Optimizer | 統計情報とコストモデルに基づいてアクセスパスとJoin順序を選べる | 第26〜29章 |
| 第5部 Transaction | 複数トランザクションを並行実行しつつ、クラッシュ後もコミット済みデータを復元できる | 第30〜35章 |
| 第6部 Server、運用、品質保証 | TCP経由でクライアントから接続できるClient/Server型のRDBMSになる | 第36〜40章 |

このうち第0〜5部の到達点は、対応する部を終えた時点ですでに本文中で確認済みです(第10章「第1部の到達点」、第16章「第2部の到達点」、第25章「第3部の到達点」、第29章「到達点」、第35章「第5部の到達点」)。
第6部の到達点、「TCP経由でクライアントから接続できるClient/Server型のRDBMSになる」は第36章の時点ですでに満たされていました。
この章が積み増したのは、その主張を裏付ける根拠の総量です。
Fuzzingが字句解析器と構文解析器の頑健性を、Property-based TestがB+TreeとSQL実行の不変条件を、Crash Injection LoopがCrash Recoveryの復元力を、決定的Random ConcurrencyがStrict 2PLの直列化可能性を、それぞれ個別のシナリオではなくランダム化された多数のケースで裏付けました。

`minidb`は、この教材が第1章で「作らない」と決めた項目(分散処理、SQL標準への完全互換、認証と認可、通信の暗号化、非同期I/O)を除けば、単一ノードのRDBMSとして機能、永続化、並行制御、最適化、運用の一通りを備えたところまで到達しました。
ここから先は、発展編A(MVCC)、発展編B(高度なSQL: サブクエリ、CTE、Window Function、外部キー)、発展編C(実行エンジン: External Sort、Vectorized Execution)、発展編D(代替ストレージ: LSM-Tree)、発展編E(PostgreSQL Wire Protocol互換)という、それぞれ独立したブランチへの分岐点になります(第1章)。
どの発展編も、この章までに積み上げたテスト資産の地図の上に、新しい層を1つずつ足していく作業です。

## 演習問題

### 必須課題

1. `tests/fuzz_parser.rs`の生成器を土台に、`cargo-fuzz`用のFuzz Targetを`fuzz/fuzz_targets/parse_statement.rs`として追加してください。`libfuzzer-sys`クレートを使い、`fuzz_target!(|data: &[u8]| { ... })`の中で`String::from_utf8_lossy(data)`を`tokenize`と`parse_statement`へ渡す形になります。`cargo fuzz run parse_statement`を数分走らせ、この章の自作ファザーとは異なる入力を試せることを確認してください。
2. `tests/property_sql.rs`に3つ目のPropertyを追加してください。`UPDATE`を伴わない`JOIN`(`orders`と`customers`のような1対多)について、`JOIN`結果の行数が常に「結合条件を満たす`(customers, orders)`の組の数」と一致することを、ランダムな行の追加や削除を挟みながら確認します。
3. 本文「Benchmark」節で見つかったFree Space Mapの制約を、`src/free_space_map.rs`と`src/storage.rs`を読んで原因を特定してください。`find_candidate`が返す候補と、実際に空きのあるページの一覧を突き合わせるテストを書き、原因を再現するテストとして残してください(修正は発展課題)。

### 発展課題

1. 必須課題3で再現したFree Space Mapの制約を実際に修正してください。修正後、`tests/bench.rs`の`accounts_db`を2列の定義へ戻しても`micro_point_lookup`が`DbError::CatalogTooLarge`にならないことを確認してください。
2. `tests/bench.rs`の`small_oltp_transfer_benchmark`を、第35章の実スレッドハーネス(`SharedDatabase`、`std::thread::spawn`)を使う並行版へ拡張してください。スレッド数を2、4、8と増やしたときのスループット(tps)の変化を実測し、Strict 2PLのロック競合がスケーラビリティにどう効くかを考察してください。
3. `cargo flamegraph`(または`perf record`/`perf report`)を使い、`join_aggregate_benchmark`のプロファイルを取ってください。実行時間の大部分がHash Joinの構築、`GROUP BY`のHash Aggregate、`SELECT`の行コピーのどこに落ちているかを特定し、最も時間を使っている箇所への最適化案(コピーの削減、ハッシュ関数の変更など)を1つ設計してください。
