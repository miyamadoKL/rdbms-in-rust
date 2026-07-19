# 第27章 統計情報とCardinality Estimation

```console
minidb> CREATE TABLE orders (id BIGINT NOT NULL, status BIGINT NOT NULL, amount BIGINT NOT NULL);
CREATE TABLE
minidb> EXPLAIN SELECT id FROM orders WHERE status = 1;
QUERY PLAN
----------
Projection(id) rows=5
  └─ Filter(status = 1) rows=5
    └─ SeqScan(orders) rows=1000
(3 rows)
```

`orders`にはまだ1行も入っていません。
それでも`EXPLAIN`は`rows=1000`、`rows=5`という具体的な数を返してきます。
この数はどこから来たのでしょうか。

20行だけ挿入し、実際の分布を`ANALYZE`で調べさせてから、同じ`EXPLAIN`をもう一度実行してみます。

```console
minidb> INSERT INTO orders VALUES (0, 0, 0), (1, 0, 5), (2, 0, 10), (3, 0, 15), (4, 0, 20), \
  (5, 0, 25), (6, 0, 30), (7, 0, 35), (8, 0, 40), (9, 1, 45), (10, 0, 50), (11, 0, 55), \
  (12, 0, 60), (13, 0, 65), (14, 0, 70), (15, 0, 75), (16, 0, 80), (17, 0, 85), (18, 0, 90), (19, 1, 95);
INSERT 20
minidb> ANALYZE orders;
ANALYZE 1
minidb> EXPLAIN SELECT id FROM orders WHERE status = 1;
QUERY PLAN
----------
Projection(id) rows=2
  └─ Filter(status = 1) rows=2
    └─ SeqScan(orders) rows=20
(3 rows)
```

`status`列は20行のうち2行だけが`1`で、`ANALYZE`後の`rows=2`はその実データの分布をそのまま反映した見積もりです。
`ANALYZE`前の`rows=5`(全体の0.5%という当てずっぽうの既定値)とは違い、`SeqScan(orders)`自体の見積もりも`rows=1000`から`rows=20`(実際の行数)へ変わっています。
この章では、`rows=2`という数がどう計算されるのか、そしてその数がどこまで信じられるのかを扱います。

## 前章の限界: 「どちらが速いか」に答えられない

第26章のルールベース最適化は、`1 = 1`のような冗長な項を消し、`customers.id = 1`のような片側だけの条件を該当するテーブルまで運びました。
どちらの書き換えも、**書き換えの前後で常に同じか、それより少ない行数しか扱わない**という性質を持っていたからこそ、無条件に適用してよいと言えました。
`Predicate Pushdown`は常に得なので、迷う余地がありません。

第25章のアクセスパス選択は、事情が違います。
`customers`(50行)と`orders`(`m`行)を結合するとき、`orders.customer_id`に索引があれば`physical_plan::optimize`はIndex Nested Loop Joinを選びました。
この選択が常に得かどうかを実測したところ、答えは「データ次第」でした。

```console
$ cargo test --release --lib -- --ignored --nocapture index_nested_loop_join_is_not_always_faster
selective m= 32000  matches=    33  IndexNestedLoopJoin=5.116327ms  HashJoin=55.265092ms
dense     m= 32000  matches= 32000  IndexNestedLoopJoin=786.240948ms  HashJoin=48.669114ms
```

一致する行がごく一部にとどまる**選択的な結合**ではIndex Nested Loop Joinが10倍以上速く、`orders`のほぼ全行が一致する**密な結合**ではHash Joinの15倍以上遅くなります(第25章)。
`orders.customer_id`に索引があるという構文的な事実だけを見ている限り、この2つの場面は区別できません。
`physical_plan::optimize`(第22章、第25章)は、まさにその構文的な事実だけでIndex Nested Loop Joinを選ぶルールになっており、選択的か密かを一切見ていません。

区別に要るのは、「`orders.customer_id = <外側の値>`という条件が、実際に何行を返すか」という問いへの答えです。
この問いは、SQLの構文からは決まりません。
`orders`にどんな値がどれだけ分布しているかという、**実データについての情報**が要ります。
この章はその情報(統計情報)を集める部分を作り、次章(第28章)がその情報を使って初めて「どちらが速いか」に数値で答えます。

## 統計収集: 行数、NULL数、NDV、Min/Max、Histogram

統計情報の型は`src/statistics.rs`に置きます。
集めるのは、テーブルの行数と、列ごとの4種類の値です。

```rust
pub struct TableStats {
    pub row_count: u64,
    pub columns: Vec<ColumnStats>,
}

pub struct ColumnStats {
    pub null_count: u64,
    pub distinct_count: u64,
    pub min: Option<Value>,
    pub max: Option<Value>,
    pub histogram: Vec<Bucket>,
}
```

`min`、`max`、`histogram`の対象は、[`crate::types::compare_values`](第21章の`ORDER BY`、`MIN`/`MAX`が使う全順序)で比較できる列すべてです。
`BOOLEAN`、`BIGINT`、`TEXT`のいずれも、この全順序の上で最小値、最大値、バケツ境界を持てます。

集計は`StatsCollector`という1つの構造体が担います。

```rust
pub struct StatsCollector {
    row_count: u64,
    columns: Vec<ColumnAccumulator>,
}

impl StatsCollector {
    pub fn new(schema: &Schema) -> Self { /* ... */ }
    pub fn add_row(&mut self, tuple: &Tuple) { /* ... */ }
    pub fn finish(self) -> TableStats { /* ... */ }
}
```

`add_row`を1行ずつ呼び、最後に`finish`を呼ぶという形にしてあるのは、テーブルを`Vec<Tuple>`へ丸ごと読み込んでから集計するのではなく、`SeqScan`相当の走査(後述の`ANALYZE`)が返す行を1件ずつ流し込めるようにするためです。

### Distinct値数はHyperLogLogを使わず、`HashSet`で正確に数える

`distinct_count`(NDV、Number of Distinct Values)は、大規模なテーブルでは`HashSet`のような厳密な方法では収まらないことがよく知られています。
値の種類が数百万を超えるテーブルでは、行1件ごとに`HashSet`へ挿入する処理も、`HashSet`自体が使うメモリも無視できなくなり、実務のRDBMSの多くはHyperLogLogのような近似アルゴリズム(数KBのメモリで誤差数%のNDVを見積もる)を採用しています。

この章はその近似を採らず、`HashSet`による厳密なカウントで済ませます。

```rust
struct ColumnAccumulator {
    null_count: u64,
    distinct: HashSet<Value>,
    // ...
}
```

理由は2つあります。
1つは、この章の目的が「Cardinality Estimationの推定式そのものが正しく機能するか」を確かめることにあり、そこへHyperLogLogの推定誤差というもう1つの不確実性を持ち込みたくないことです。
推定式を検証している最中に結果がずれても、それが推定式自体の問題なのか、入力に使ったNDVの近似誤差なのかを切り分けられなくなります。
もう1つは、HyperLogLogがハッシュ関数、レジスタ配列、調和平均による推定式を要する、この教材の1章に見合わない複雑さを持つことです。
`HashSet`によるNDVの厳密な計算は、大規模データに対してはスケールしないという限界を明確に持ちながらも、実装は数行に収まり、推定式の検証という当面の目的には十分です。
近似アルゴリズムへの置き換えは、章末の演習課題に譲ります。

### Histogramは等頻度(equi-depth)を選ぶ

Histogramは、列の値の分布を`HISTOGRAM_BUCKET_COUNT`(固定で10)個のバケツに区切って持ちます。

```rust
pub const HISTOGRAM_BUCKET_COUNT: usize = 10;

pub struct Bucket {
    pub lower: Value,
    pub upper: Value,
    pub row_count: u64,
}
```

バケツの境界の決め方には、大きく2通りあります。
**等幅(equi-width)**は値の取りうる範囲を等分し、各バケツの区間の幅を揃えます。
**等頻度(equi-depth)**は逆に、各バケツに収まる行数をできるだけ揃え、区間の幅はバケツごとに変わります。

この章は等頻度を選びます。
理由は、一部の値に行が集中する歪んだ分布での精度です。
`status`列のように、1000行のうち900行が`0`、残り100行が1〜9のどれかというデータを考えます。

```rust
#[test]
fn histogram_stays_equal_depth_under_a_skewed_distribution() {
    // 0が90回、1〜9がそれぞれ1回ずつ出現する、値に偏りのある分布。
    let mut values = vec![Value::BigInt(0); 90];
    values.extend((1..10).map(Value::BigInt));
    let stats = collect(&values);
    let histogram = &stats.columns[0].histogram;
    // 等幅Histogramなら、値0のバケツに90行すべてが押し込まれる。
    // 等頻度Histogramでは、行数はどのバケツもほぼ均等(9または10)になる。
    for bucket in histogram {
        assert!(bucket.row_count == 9 || bucket.row_count == 10);
    }
}
```

値の範囲(`0`〜`9`)を等幅に10分割すれば、`0`だけのバケツに90行、残り9個のバケツに1行ずつという、ほとんど意味のないHistogramになります。
`col = 0`の選択率を「そのバケツの行数の割合」から見積もる仕組みである以上、1つのバケツに大半の行が押し込まれてしまっては、バケツを作った意味がありません。
等頻度なら、行数の多い`0`のまわりに自然とバケツが密集し(区間の幅が狭くなり)、残りのまばらな値は少ないバケツで間に合います。
PostgreSQLの`ANALYZE`が作る`pg_stats.histogram_bounds`も、同じ理由で等頻度方式を採っています。

バケツの組み立ては、ソート済みの非NULL値を`HISTOGRAM_BUCKET_COUNT`個の区間へ、できるだけ均等に割ります。

```rust
fn build_equi_depth_histogram(sorted_values: &[Value]) -> Vec<Bucket> {
    if sorted_values.is_empty() {
        return Vec::new();
    }
    let total = sorted_values.len();
    let bucket_count = total.min(HISTOGRAM_BUCKET_COUNT);
    let base_size = total / bucket_count;
    let remainder = total % bucket_count;

    let mut buckets = Vec::with_capacity(bucket_count);
    let mut start = 0;
    for i in 0..bucket_count {
        let size = base_size + usize::from(i < remainder);
        let end = start + size;
        let chunk = &sorted_values[start..end];
        buckets.push(Bucket {
            lower: chunk.first().expect("sizeは1以上").clone(),
            upper: chunk.last().expect("sizeは1以上").clone(),
            row_count: chunk.len() as u64,
        });
        start = end;
    }
    buckets
}
```

割り切れない余りは先頭のバケツから1行ずつ多めに配るだけの単純な実装ですが、値の総数が`HISTOGRAM_BUCKET_COUNT`未満なら、バケツもその数だけしかできません。

## `ANALYZE`文: 統計を集める

`ANALYZE [テーブル名]`という新しい文を追加しました。
テーブル名を省略すると、カタログに登録されている全テーブルが対象になります。

```console
minidb> ANALYZE orders;
ANALYZE 1
minidb> ANALYZE;
ANALYZE 3
```

構文解析(`src/lexer.rs`の`Keyword::Analyze`、`src/parser.rs`の`parse_analyze_statement`)は`CREATE TABLE`、`DROP TABLE`と同じくらい単純です。
束縛(`src/binder.rs`)も、`DropTable`と同じ方針を採りました。
テーブル名が指定されていれば、その存在だけをここで確認し(未知のテーブル名は位置情報付きの`DbError::Bind`になります)、ASTのバリアントをそのまま`BoundStatement::Analyze`として通します。
`ANALYZE`は既存のカタログエントリの統計欄を書き換えるだけの操作であり、`CREATE INDEX`、`DROP INDEX`(第24章)と同じく、式の名前解決や型検査を必要としないからです。

実行(`Database::execute_analyze`)は、対象テーブルを`SeqScan`と同じ経路で1回走査します。

```rust
fn collect_table_stats(&self, table_id: TableId, table_name: &str, schema: &Schema) -> DbResult<TableStats> {
    let plan = PhysicalPlan::SeqScan(SeqScanNode {
        table_id,
        table_name: table_name.to_string(),
        schema: schema.clone(),
    });
    let mut executor = self.build_query_executor(&plan, None)?;
    let mut collector = StatsCollector::new(schema);
    while let Some(tuple) = executor.next()? {
        collector.add_row(&tuple);
    }
    Ok(collector.finish())
}
```

`PhysicalPlan::SeqScan`を直接組み立て、`build_query_executor`(第19章から`SELECT`が使っているのと同じ関数)へ渡しているだけです。
`Backend::Memory`、`Backend::Disk`のどちらであっても、`SeqScan`の実行(`MemSeqScanExec`、`DiskSeqScanExec`)がすでにこの違いを吸収しているため、`ANALYZE`の側でバックエンドを意識する必要はありません。

## 統計の保存とデフォルト選択率

`Backend::Memory`の統計は、`Catalog`、`MemStorage`と同じくプロセスのメモリ上だけに保持し、永続化しません。

```rust
enum Backend {
    Memory {
        catalog: Catalog,
        storage: MemStorage,
        stats: HashMap<TableId, TableStats>,
    },
    Disk {
        storage: Box<Storage>,
    },
}
```

`Backend::Disk`は、`src/storage.rs`のCatalogページへ統計情報を追記します。
索引メタデータ(第24章)が末尾にセクションを追加したのと同じパターンで、既存のレイアウトの末尾に`stats`セクションを追記するだけです。

```text
stats_count: u32
stats × stats_count:
    table_id:      u64
    row_count:     u64
    column_count:  u16
    columns × column_count:
        null_count:     u64
        distinct_count: u64
        min:            Value
        max:            Value
        bucket_count:   u16
        buckets × bucket_count:
            lower:     Value
            upper:     Value
            row_count: u64
```

`Value`(`NULL`/`BOOLEAN`/`BIGINT`/`TEXT`)は、既存の`data_type_to_u8`(列の**型**だけを表す1バイト)とは別に、値そのものを復元できる自己記述形式でエンコードします。

```rust
fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0),
        Value::Boolean(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Value::BigInt(n) => {
            out.push(2);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(3);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
}
```

`min`と`max`は`Option<Value>`ですが、`StatsCollector`はそもそも`NULL`値を`min`/`max`の対象に含めないため、「値が1件も無い」(`None`)と`Value::Null`が同時に起こることはありません。
この不変条件を使い、`None`を`Value::Null`と同じタグ(`0`)で表すことで、存在を示す専用のフラグバイトを別に持たずに済ませています。

このセクションを追加したことに伴い、`CATALOG_LAYOUT_VERSION`を`1`から`2`へ上げました。
モジュール冒頭のドキュメントコメント(第24章までの節と同じ形式)にも、この変更を書き残してあります。
章をまたいだファイル互換性を約束しない方針(第15章から一貫)はそのままで、`CATALOG_MAGIC`と`CATALOG_LAYOUT_VERSION`による判定が、レイアウトの変わった古いカタログを確実に拒否します。

`ANALYZE`を一度も実行していないテーブルは、統計を持ちません。
この場合の推定は、選択率の慣用定数にフォールバックします。

```rust
/// 等値述語のデフォルト選択率(統計が無い場合)。出典は本文を参照。
pub const DEFAULT_EQ_SEL: f64 = 0.005;

/// 不等号述語のデフォルト選択率(統計が無い場合)。出典は本文を参照。
pub const DEFAULT_INEQ_SEL: f64 = 1.0 / 3.0;
```

この2つの値は、PostgreSQLの`selfuncs.c`が同じ役割で使っている定数(`DEFAULT_EQ_SEL`と`DEFAULT_INEQ_SEL`)から取っています。
`DEFAULT_EQ_SEL`(0.5%)は、「典型的なテーブルでは、ある列がある1つの値と一致する行はおよそ200行に1行だろう」という経験則です。
`DEFAULT_INEQ_SEL`(1/3)は、「範囲述語(`<`や`>`)は、値の分布に関する手がかりが無くても、全体のおよそ3分の1を切り出すだろう」という経験則です。
どちらも統計的に導出された値ではなく、実際のワークロードで長年使われてきた経験則としての妥当性から採用されています。
章の冒頭で見た`rows=5`(`ANALYZE`前の`WHERE status = 1`、1000行のテーブルに対して)は、まさにこの`DEFAULT_EQ_SEL`(0.005)を1000に掛けた値です。

## Cardinality Estimation: 推定式

推定式は`src/estimator.rs`に集めます。
どの式も、`Option<&ColumnStats>`(統計が無ければ`None`)を受け取り、選択率(0.0〜1.0の`f64`)または行数を返す、単純な関数です。

### 等値述語: `col = 定数`

Histogramがあれば、定数が収まるバケツを探し、そのバケツの行数の割合を、バケツ内のDistinct値数(全体のNDVをバケツ数で均等割りした近似)で割ります。

```rust
pub fn estimate_equality_selectivity(stats: Option<&ColumnStats>, value: &Value) -> f64 {
    let Some(stats) = stats else { return DEFAULT_EQ_SEL };

    if !stats.histogram.is_empty() {
        let total_rows: u64 = stats.histogram.iter().map(|b| b.row_count).sum();
        if total_rows == 0 {
            return DEFAULT_EQ_SEL;
        }
        let bucket_count = stats.histogram.len() as f64;
        let ndv_per_bucket = (stats.distinct_count as f64 / bucket_count).max(1.0);
        for bucket in &stats.histogram {
            if compare_values(value, &bucket.lower) != Ordering::Less && compare_values(value, &bucket.upper) != Ordering::Greater {
                let bucket_fraction = bucket.row_count as f64 / total_rows as f64;
                return bucket_fraction / ndv_per_bucket;
            }
        }
        return DEFAULT_EQ_SEL; // どのバケツにも収まらない(観測範囲の外)
    }

    if stats.distinct_count > 0 { 1.0 / stats.distinct_count as f64 } else { DEFAULT_EQ_SEL }
}
```

Histogramが無ければ`1 / NDV`(「NDV個の値が一様に分布している」という仮定)、NDVも無ければ`DEFAULT_EQ_SEL`という3段階のフォールバックです。

### 範囲述語: `col > / >= / < / <= 定数`

各バケツについて、「そのバケツの両端がどちらも述語を満たすか」で3通りに分けます。
両端とも満たせばバケツ全体を選択率1.0として数え、両端とも満たさなければ0.0、片方だけ満たすなら「バケツ内で値は一様に分布している」と仮定して0.5とします。

```rust
fn bucket_overlap_fraction(op: RangeOp, value: &Value, lower: &Value, upper: &Value) -> f64 {
    let satisfies = |x: &Value| -> bool {
        let ord = compare_values(x, value);
        match op {
            RangeOp::Gt => ord == Ordering::Greater,
            RangeOp::Ge => ord != Ordering::Less,
            RangeOp::Lt => ord == Ordering::Less,
            RangeOp::Le => ord != Ordering::Greater,
        }
    };
    match (satisfies(lower), satisfies(upper)) {
        (true, true) => 1.0,
        (false, false) => 0.0,
        _ => 0.5,
    }
}
```

Histogramがあれば、この関数をバケツごとに呼び、行数で重み付けして合計します。
無ければMin/Maxを1個のバケツとみなして同じ計算をする(線形補間に相当する)というフォールバックで、Min/Maxも無ければ`DEFAULT_INEQ_SEL`です。
`compare_values`による大小比較だけで組み立ててあるので、数値(`BIGINT`)だけでなく`TEXT`、`BOOLEAN`にもそのまま使えます。

### `AND` / `OR` / `NOT`

`AND`は独立性を仮定した積、`OR`は包除原理、`NOT`は1引く元の値です。

```rust
pub fn estimate_and_selectivity(a: f64, b: f64) -> f64 {
    (a * b).clamp(0.0, 1.0)
}

pub fn estimate_or_selectivity(a: f64, b: f64) -> f64 {
    (1.0 - (1.0 - a) * (1.0 - b)).clamp(0.0, 1.0)
}

pub fn estimate_not_selectivity(a: f64) -> f64 {
    (1.0 - a).clamp(0.0, 1.0)
}
```

`AND`の独立性の仮定(2つの述語が互いに無関係に成り立つ)は、`status`と`amount`のように実際には相関する列の組み合わせでは崩れます。
この節ではまだ崩れないことにして、崩れる実例は後の節で`EXPLAIN ANALYZE`を使って確かめます。

### Join Cardinality

等値結合(第22章、第25章のJoinが前提とする範囲)の結果行数は、標準的な式`|L| × |R| / max(NDV_l, NDV_r)`で見積もります。

```rust
pub fn estimate_join_row_count(left_rows: u64, right_rows: u64, left_ndv: u64, right_ndv: u64) -> u64 {
    let max_ndv = left_ndv.max(right_ndv).max(1);
    ((left_rows as u128 * right_rows as u128) / max_ndv as u128).min(u64::MAX as u128) as u64
}
```

結合キーの片方が主キー(NDVが行数と一致する一意な列)であれば、この式は「外側の各行に対して内側がちょうど1行だけ一致する」という主キーと外部キーの結合の典型的な状況にちょうど一致します。
`HashJoin`は`keys`(等値条件の対の並び、第22章)から結合キー列の実際のNDVを引けるため、`ANALYZE`済みならこの式をそのまま使います。
`NestedLoopJoin`は任意の条件(等号とは限らない)を持ち、結合キー列を機械的に特定できないため、「値はすべて一意」という最も楽観的な既定値(それぞれの出力行数そのもの)にフォールバックします。

### Aggregate後の行数

`GROUP BY`後の行数は、`GROUP BY`に並ぶ各列のNDVの積で見積もります。

```rust
pub fn estimate_aggregate_row_count(group_ndvs: &[u64], input_rows: u64) -> u64 {
    if group_ndvs.is_empty() {
        // GROUP BYが無い集約(集約関数だけのSELECT)は、常にちょうど1行になる。
        return 1;
    }
    let mut product: u128 = 1;
    for &ndv in group_ndvs {
        product = product.saturating_mul(u128::from(ndv.max(1)));
        if product >= u128::from(input_rows) {
            return input_rows;
        }
    }
    product.min(u128::from(input_rows)) as u64
}
```

`dept`と`status`という2つの列で`GROUP BY`すれば、グループの上限は理屈のうえでは`NDV(dept) × NDV(status)`です。
ただし1グループは少なくとも1行を含むので、グループ数が入力行数を超えることはあり得ません。
積が`input_rows`を超えた時点で頭打ちにし(`u64`の掛け算オーバーフローを避けるため`u128`で計算します)、実際に組み合わせが少ない(相関がある)場合の過大評価を防ぎます。

### この章の推定はまだプラン選択に使わない

ここまでの推定式は、`EXPLAIN`と`EXPLAIN ANALYZE`の表示にだけ使います。
`physical_plan::optimize`(第22章、第25章)のアクセスパス選択とJoin方式選択のロジックはこの章では変更していません。
Index Nested Loop Joinを選ぶかHash Joinを選ぶかは、引き続き「索引があるかどうか」という構文的な性質だけで決まります。
この章が用意するのは、次章(第28章)がコストを見積もるための材料です。
コストという1つの数値へ複数の推定行数を合成し、実際にプラン選択へ反映させるのは第28章の仕事です。

## `EXPLAIN`と`EXPLAIN ANALYZE`

`EXPLAIN`(推定のみ)は、既存の`write_tree`が組み立てるラベルの末尾に、`rows=<推定値>`(先頭に半角スペース)を追記した形で表示します。

```console
minidb> EXPLAIN SELECT id FROM orders WHERE amount > 4000;
QUERY PLAN
----------
Projection(id) rows=150
  └─ Filter(amount > 4000) rows=150
    └─ SeqScan(orders) rows=1000
(3 rows)
```

`EXPLAIN ANALYZE`(PostgreSQLの`EXPLAIN ANALYZE`に相当)は、対象の文を実際に実行し、`actual=<実測値>`(こちらも先頭に半角スペース)を推定値と並べて表示します。

```console
minidb> EXPLAIN ANALYZE SELECT id FROM orders WHERE amount > 4000;
QUERY PLAN
----------
Projection(id) rows=150 actual=189
  └─ Filter(amount > 4000) rows=150 actual=189
    └─ SeqScan(orders) rows=1000 actual=1000
(3 rows)
```

実測行数は、`Box<dyn Executor>`(第19章)を`CountingExec`という薄いラッパーで包むことで集めます。

```rust
pub struct CountingExec<'a> {
    inner: Box<dyn Executor + 'a>,
    count: Rc<Cell<u64>>,
}

impl<'a> Executor for CountingExec<'a> {
    fn output_schema(&self) -> &Schema {
        self.inner.output_schema()
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        let result = self.inner.next()?;
        if result.is_some() {
            self.count.set(self.count.get() + 1);
        }
        Ok(result)
    }
}
```

`build_query_executor`(第19章から`SELECT`の実行を担う関数)が`PhysicalPlan`の木をたどって`Executor`の木を組み立てるとき、各ノードを`CounterNode`(`PhysicalPlan::children()`と同じ形を持つ、カウンタだけの木)に対応する`CountingExec`で包みます。
カウンタを`Rc<Cell<u64>>`で持つのは、`Executor`の木を最後まで実行し終えて(=`Box<dyn Executor>`の所有権が尽きて)からでないと、`Database::execute_explain`側が各カウンタの値を読めないためです。
`&mut u64`のような通常の参照では、`Executor`の木を借用したまま結果を読み出すことになり、借用規則に反します。

`SELECT`は`Executor`の木を最後まで実行できるので、全ノードに`actual=`が付きます。
`INSERT`、`UPDATE`、`DELETE`は事情が違います。
`physical_plan`モジュール冒頭で説明されているとおり、この3つは`Executor`を経由しない一括処理(`run_insert`、`run_update`、`run_delete`)として実装されており、演算子ごとの内訳を計測する手段がありません。
そのためこの章では、`INSERT`、`UPDATE`、`DELETE`の`EXPLAIN ANALYZE`は**根のノード1行にだけ**`actual=`(実際に書き込まれた行数)を添え、`input`側(`Values`、`Scan`)のサブツリーは推定行数のみを表示します。

```console
minidb> EXPLAIN ANALYZE INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');
QUERY PLAN
----------
Insert(users) rows=2 actual=2
  └─ Values(2 rows) rows=2
(2 rows)

minidb> EXPLAIN ANALYZE UPDATE users SET name = 'x' WHERE id = 1;
QUERY PLAN
----------
Update(users) rows=1000 actual=1
  └─ SeqScan(users) rows=1000
(2 rows)
```

もう1つ明記しておく必要があるのは、`EXPLAIN ANALYZE INSERT`/`UPDATE`/`DELETE`は**実際に書き込みを行う**という点です。
PostgreSQLの`EXPLAIN (ANALYZE, ...)`と異なり、この教材はトランザクション内で計測だけを取り消す機能を持たないため(トランザクションの導入はまだ先の章です)、`EXPLAIN ANALYZE INSERT`を実行すればテーブルの行は実際に増えます。

## 推定と実測のずれ: 相関する列

`AND`の推定式(独立性を仮定した積)は、2つの列が実際には相関しているとき、実測から大きくずれます。
`status`列と`amount`列に強い相関を持たせたテーブルで確かめます。

```console
minidb> CREATE TABLE orders (id BIGINT NOT NULL, status BIGINT NOT NULL, amount BIGINT NOT NULL);
CREATE TABLE
minidb> -- statusが1の行は必ずamountが5000以上、0の行は必ずamountが50未満になるよう1000行挿入
minidb> ANALYZE orders;
ANALYZE 1
minidb> EXPLAIN ANALYZE SELECT id FROM orders WHERE status = 1 AND amount > 100;
QUERY PLAN
----------
Projection(id) rows=10 actual=100
  └─ Filter(status = 1 AND amount > 100) rows=10 actual=100
    └─ SeqScan(orders) rows=1000 actual=1000
(3 rows)
```

`status = 1`はちょうど100行(全体の10%)に一致し、その100行は`amount`が必ず5000以上なので`amount > 100`も常に成り立ちます。
実測(`actual=100`)は、この「`status = 1`ならば必ず`amount > 100`」という関係をそのまま反映しています。
推定式は`estimate_and_selectivity(sel(status = 1), sel(amount > 100))`という積を計算しますが、これは`status`と`amount`が互いに無関係に決まるという仮定のもとでの見積もりです。
実際には`status = 1`という条件が`amount > 100`をほぼ確定させてしまうため、2つの条件は独立ではなく、積による見積もり(`rows=10`)は実測(`actual=100`)の10分の1にとどまります。

この種のずれは、列同士の相関を持たない統計情報(列ごとのHistogramだけを持ち、列の組み合わせの分布は持たない)を使う限り避けられません。
複数列の相関を捉えるには、列の組み合わせごとのHistogram(Multi-column Statistics)のような、この章より進んだ統計情報が要ります。
`EXPLAIN ANALYZE`で`rows=`と`actual=`を並べて見せる仕組みは、こうした推定の限界を隠さず、どこでどれだけずれているかを利用者が確認できるようにするためのものです。

## 到達点

`ANALYZE`はテーブルの行数、列ごとのNULL数、NDV、Min/Max、等頻度Histogramを集め、`Backend::Disk`ではCatalogページへ永続化されます。
等値、範囲、`AND`、`OR`、`NOT`、Join、`GROUP BY`後の行数という7種類の推定式が、この統計情報(または統計が無いときのデフォルト選択率)から行数を見積もり、`EXPLAIN`の`rows=`として表示されます。
`EXPLAIN ANALYZE`は実際に実行し、`actual=`で実測値と並べます。
相関する列の例が示すとおり、この推定はいつでも正しいわけではありません。

それでも、これで「選択的な結合」と「密な結合」を区別する材料は揃いました。
第28章は、この章が用意した推定行数を、I/OとCPUのコストへ変換し、複数のアクセスパスとJoin方式を数値で比較して選ぶコストベース最適化を扱います。

## 演習問題

### 必須課題

1. `estimate_equality_selectivity`は、Histogramのどのバケツにも収まらない値(観測範囲の外の値)に対して`DEFAULT_EQ_SEL`にフォールバックします。この挙動を、`ANALYZE`済みのテーブルに対して`WHERE id = <Max値より大きい定数>`を実行し、`EXPLAIN`の`rows=`が`DEFAULT_EQ_SEL`から計算した値になることを確認するテストで検証してください。
2. `estimate_aggregate_row_count`は`GROUP BY`の各列のNDVが独立だと仮定して積を取ります。`dept`と`dept_name`(`dept`ごとに1対1で決まる列)のように、実際には従属関係にある2列で`GROUP BY dept, dept_name`した場合、この仮定がどう破綻するかを実測し、`AND`の推定と同じ種類の限界であることを`EXPLAIN ANALYZE`を使って確認してください。
3. `StatsCollector::finish`は、非NULLの値をすべて`Vec<Value>`として保持してからソートし、Histogramを組み立てます(`distinct`とは別に、複製した値をもう1セット持ちます)。行数が数百万に達するテーブルでこのメモリ使用量がどう振る舞うか、`std::mem::size_of::<Value>()`を手がかりに見積もり、必要ならソート済みの一時ファイルへ退避するなど、メモリ使用量を減らす設計を検討してください(実装は必須ではありません)。
4. `IndexNestedLoopJoin`(第25章)の推定は、内側テーブルの列のNDVから「平均的な等値検索は何行返すか」(`1 / NDV`)を見積もっています(本文の`explain_text`の実装を参照)。この見積もりが、索引の値の分布が偏っている(一部の値に検索が集中する)場合にどれだけ実測とずれるかを、`ANALYZE`済みのテーブルと`EXPLAIN ANALYZE`を使って確認してください。

### 発展課題

1. この章のNDVは`HashSet`による厳密な計算です。HyperLogLogのような近似アルゴリズムを実際に実装し、行数を変えながら真のNDVとの誤差率を測定してください。行数が少ないうちは近似アルゴリズムの固定オーバーヘッド(レジスタ配列のメモリ)のほうが`HashSet`より不利になる分岐点がどこにあるかも確認してください。
2. `estimate_range_selectivity`のバケツ内一様分布の仮定(`bucket_overlap_fraction`が境界をまたぐバケツを一律0.5とする)を、バケツの区間幅と定数の位置から線形補間する(数値型に限る)より精密な見積もりへ改善し、`HISTOGRAM_BUCKET_COUNT`を変えながら推定誤差がどう変化するかを実測してください。
3. 本文で触れた列同士の相関(Multi-column Statistics)を、簡易な形(2列の組み合わせごとのNDVだけを追加で持つなど)で実装し、「相関する列」の節で見た`rows=10 actual=100`というずれがどこまで縮まるかを確認してください。
